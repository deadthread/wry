// Copyright 2020-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

mod download;
#[cfg(target_os = "macos")]
mod drag_drop;
mod navigation;
#[cfg(feature = "mac-proxy")]
mod proxy;
#[cfg(target_os = "macos")]
mod synthetic_mouse_events;
mod util;

mod class;
use class::url_scheme_handler;
use class::wry_download_delegate::WryDownloadDelegate;
pub use class::wry_web_view::WryWebView;
use class::wry_web_view::WryWebViewIvars;
use class::wry_web_view_delegate::{WryWebViewDelegate, IPC_MESSAGE_HANDLER_NAME};
use class::wry_web_view_parent::WryWebViewParent;
use class::wry_web_view_ui_delegate::WryWebViewUIDelegate;
use class::{document_title_changed_observer::*, wry_navigation_delegate::WryNavigationDelegate};

use dpi::{LogicalPosition, LogicalSize};
use objc2::{
  rc::Retained,
  runtime::{AnyObject, Bool, NSObject, ProtocolObject},
  ClassType, DeclaredClass,
};
use objc2_app_kit::{
  NSApplication, NSAutoresizingMaskOptions, NSTitlebarSeparatorStyle, NSView, NSWindow,
};
use objc2_foundation::{
  ns_string, CGPoint, CGRect, CGSize, MainThreadMarker, NSBundle, NSDate, NSError,
  NSJSONSerialization, NSMutableURLRequest, NSNumber, NSObjectNSKeyValueCoding, NSObjectProtocol,
  NSString, NSUTF8StringEncoding, NSURL, NSUUID,
};
#[cfg(target_os = "ios")]
use objc2_ui_kit::UIScrollView;
use objc2_web_kit::{
  WKAudiovisualMediaTypes, WKURLSchemeHandler, WKUserContentController, WKUserScript,
  WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use once_cell::sync::Lazy;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use std::{
  collections::{HashMap, HashSet},
  ffi::c_void,
  os::raw::c_char,
  panic::AssertUnwindSafe,
  ptr::null_mut,
  str,
  sync::{Arc, Mutex},
};

#[cfg(feature = "mac-proxy")]
use crate::{
  proxy::ProxyConfig,
  wkwebview::proxy::{
    nw_endpoint_t, nw_proxy_config_create_http_connect, nw_proxy_config_create_socksv5,
  },
};

use crate::{Error, Rect, RequestAsyncResponder, Result, WebContext, WebViewAttributes, RGBA};

use http::Request;

use self::util::Counter;

static COUNTER: Counter = Counter::new();
static WEBVIEW_IDS: Lazy<Mutex<HashSet<u32>>> = Lazy::new(Default::default);

#[derive(Debug, Default, Copy, Clone)]
pub struct PrintMargin {
  pub top: f32,
  pub right: f32,
  pub bottom: f32,
  pub left: f32,
}

#[derive(Debug, Default, Clone)]
pub struct PrintOptions {
  pub margins: PrintMargin,
}

pub(crate) struct InnerWebView {
  pub webview: Retained<WryWebView>,
  pub manager: Retained<WKUserContentController>,
  is_child: bool,
  pending_scripts: Arc<Mutex<Option<Vec<String>>>>,
  // Note that if following functions signatures are changed in the future,
  // all functions pointer declarations in objc callbacks below all need to get updated.
  ipc_handler_delegate: Option<Retained<WryWebViewDelegate>>,
  #[allow(dead_code)]
  // We need this the keep the reference count
  document_title_changed_observer: Option<Retained<DocumentTitleChangedObserver>>,
  #[allow(dead_code)]
  // We need this the keep the reference count
  navigation_policy_delegate: Retained<WryNavigationDelegate>,
  #[allow(dead_code)]
  // We need this the keep the reference count
  download_delegate: Option<Retained<WryDownloadDelegate>>,
  #[allow(dead_code)]
  // We need this the keep the reference count
  ui_delegate: Retained<WryWebViewUIDelegate>,
  protocol_ptrs: Vec<*mut Box<dyn Fn(Request<Vec<u8>>, RequestAsyncResponder)>>,
  webview_id: u32,
}

impl InnerWebView {
  pub fn new(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
    _web_context: Option<&mut WebContext>,
  ) -> Result<Self> {
    let ns_view = match window.window_handle()?.as_raw() {
      #[cfg(target_os = "macos")]
      RawWindowHandle::AppKit(w) => w.ns_view.as_ptr(),
      #[cfg(target_os = "ios")]
      RawWindowHandle::UiKit(w) => w.ui_view.as_ptr(),
      _ => return Err(Error::UnsupportedWindowHandle),
    };

    unsafe {
      Self::new_ns_view(
        &*(ns_view as *mut NSView),
        attributes,
        pl_attrs,
        _web_context,
        false,
      )
    }
  }

  pub fn new_as_child(
    window: &impl HasWindowHandle,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
    _web_context: Option<&mut WebContext>,
  ) -> Result<Self> {
    let ns_view = match window.window_handle()?.as_raw() {
      #[cfg(target_os = "macos")]
      RawWindowHandle::AppKit(w) => w.ns_view.as_ptr(),
      #[cfg(target_os = "ios")]
      RawWindowHandle::UiKit(w) => w.ui_view.as_ptr(),
      _ => return Err(Error::UnsupportedWindowHandle),
    };

    unsafe {
      Self::new_ns_view(
        &*(ns_view as *mut NSView),
        attributes,
        pl_attrs,
        _web_context,
        true,
      )
    }
  }

  fn new_ns_view(
    ns_view: &NSView,
    attributes: WebViewAttributes,
    pl_attrs: super::PlatformSpecificWebViewAttributes,
    _web_context: Option<&mut WebContext>,
    is_child: bool,
  ) -> Result<Self> {
    let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;

    let mut wv_ids = WEBVIEW_IDS.lock().unwrap();
    let webview_id = COUNTER.next();
    wv_ids.insert(webview_id);
    drop(wv_ids);

    // Safety: objc runtime calls are unsafe
    unsafe {
      let config = WKWebViewConfiguration::new();

      // Incognito mode
      let os_version = util::operating_system_version();
      #[cfg(target_os = "macos")]
      let custom_data_store_available = os_version.0 >= 14;
      #[cfg(target_os = "ios")]
      let custom_data_store_available = os_version.0 >= 17;

      let data_store = match (
        attributes.incognito,
        custom_data_store_available,
        pl_attrs.data_store_identifier,
      ) {
        (true, _, _) => WKWebsiteDataStore::nonPersistentDataStore(),
        // if data_store_identifier is given and custom data stores are available, use custom store
        (false, true, Some(data_store)) => {
          let identifier = NSUUID::from_bytes(data_store);
          WKWebsiteDataStore::dataStoreForIdentifier(&identifier)
        }
        // default data store
        _ => WKWebsiteDataStore::defaultDataStore(),
      };

      // Register Custom Protocols
      let mut protocol_ptrs = Vec::new();
      for (name, function) in attributes.custom_protocols {
        let url_scheme_handler_cls = url_scheme_handler::create(&name);
        let handler: *mut AnyObject = objc2::msg_send![url_scheme_handler_cls, new];
        let function = Box::into_raw(Box::new(function));
        protocol_ptrs.push(function);

        let ivar = (*handler).class().instance_variable("function").unwrap();
        let ivar_delegate = ivar.load_mut(&mut *handler);
        *ivar_delegate = function as *mut _ as *mut c_void;

        let ivar = (*handler).class().instance_variable("webview_id").unwrap();
        let ivar_delegate = ivar.load_mut(&mut *handler);
        *ivar_delegate = webview_id;

        let set_result = objc2::exception::catch(AssertUnwindSafe(|| {
          config.setURLSchemeHandler_forURLScheme(
            Some(&*(handler.cast::<ProtocolObject<dyn WKURLSchemeHandler>>())),
            &NSString::from_str(&name),
          );
        }));
        if set_result.is_err() {
          return Err(Error::UrlSchemeRegisterError(name));
        }
      }

      // WebView and manager
      let manager = config.userContentController();
      let webview = mtm.alloc::<WryWebView>().set_ivars(WryWebViewIvars {
        is_child,
        #[cfg(target_os = "macos")]
        drag_drop_handler: match attributes.drag_drop_handler {
          Some(handler) => handler,
          None => Box::new(|_| false),
        },
        #[cfg(target_os = "macos")]
        accept_first_mouse: Bool::new(attributes.accept_first_mouse),
        custom_protocol_task_ids: HashMap::new(),
      });

      config.setWebsiteDataStore(&data_store);
      let _preference = config.preferences();
      let _yes = NSNumber::numberWithBool(true);

      #[cfg(feature = "mac-proxy")]
      if let Some(proxy_config) = attributes.proxy_config {
        let proxy_config = match proxy_config {
          ProxyConfig::Http(endpoint) => {
            let nw_endpoint = nw_endpoint_t::try_from(endpoint).unwrap();
            nw_proxy_config_create_http_connect(nw_endpoint, null_mut())
          }
          ProxyConfig::Socks5(endpoint) => {
            let nw_endpoint = nw_endpoint_t::try_from(endpoint).unwrap();
            nw_proxy_config_create_socksv5(nw_endpoint)
          }
        };

        let proxies: Retained<objc2_foundation::NSArray<NSObject>> =
          objc2_foundation::NSArray::arrayWithObject(&*proxy_config);
        data_store.setValue_forKey(Some(&proxies), ns_string!("proxyConfigurations"));
      }

      _preference.setValue_forKey(
        Some(&_yes),
        ns_string!("allowsPictureInPictureMediaPlayback"),
      );

      #[cfg(target_os = "ios")]
      config.setValue_forKey(Some(&_yes), ns_string!("allowsInlineMediaPlayback"));

      if attributes.autoplay {
        config.setMediaTypesRequiringUserActionForPlayback(
          WKAudiovisualMediaTypes::WKAudiovisualMediaTypeNone,
        );
      }

      #[cfg(feature = "transparent")]
      if attributes.transparent {
        let no = NSNumber::numberWithBool(false);
        // Equivalent Obj-C:
        config.setValue_forKey(Some(&no), ns_string!("drawsBackground"));
      }

      #[cfg(feature = "fullscreen")]
      // Equivalent Obj-C:
      _preference.setValue_forKey(Some(&_yes), ns_string!("fullScreenEnabled"));

      #[cfg(target_os = "macos")]
      let webview = {
        let window = ns_view.window().unwrap();

        let scale_factor = window.backingScaleFactor();
        let (x, y) = attributes
          .bounds
          .map(|b| b.position.to_logical::<f64>(scale_factor))
          .map(Into::into)
          .unwrap_or((0, 0));
        let (w, h) = if is_child {
          attributes
            .bounds
            .map(|b| b.size.to_logical::<u32>(scale_factor))
            .map(Into::into)
        } else {
          None
        }
        .unwrap_or_else(|| {
          if is_child {
            let frame = NSView::frame(ns_view);
            (frame.size.width as u32, frame.size.height as u32)
          } else {
            (0, 0)
          }
        });

        let frame = CGRect {
          origin: if is_child {
            window_position(ns_view, x, y, h as f64)
          } else {
            CGPoint::new(x as f64, (0 - y - h as i32) as f64)
          },
          size: CGSize::new(w as f64, h as f64),
        };
        let webview: Retained<WryWebView> =
          objc2::msg_send_id![super(webview), initWithFrame:frame configuration:&**config];
        webview
      };
      #[cfg(target_os = "ios")]
      let webview = {
        let frame = ns_view.frame();
        let webview = WKWebView::initWithFrame_configuration(webview, frame, &config);
        webview
      };

      #[cfg(target_os = "macos")]
      {
        if is_child {
          // fixed element
          webview.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewMinYMargin);
        } else {
          // Auto-resize
          let options = NSAutoresizingMaskOptions(
            NSAutoresizingMaskOptions::NSViewHeightSizable.0
              | NSAutoresizingMaskOptions::NSViewWidthSizable.0,
          );
          webview.setAutoresizingMask(options);
        }

        // allowsBackForwardNavigation
        webview.setAllowsBackForwardNavigationGestures(attributes.back_forward_navigation_gestures);

        // tabFocusesLinks
        _preference.setValue_forKey(Some(&_yes), ns_string!("tabFocusesLinks"));
      }
      #[cfg(target_os = "ios")]
      {
        // set all autoresizingmasks
        webview.setAutoresizingMask(NSAutoresizingMaskOptions::from_bits(31).unwrap());
        // let () = msg_send![webview, setAutoresizingMask: 31];

        // disable scroll bounce by default
        let scroll_view: UIScrollView = webview.scrollView(); // FIXME: not test yet
        scroll_view.setBounces(false)
      }

      if !attributes.visible {
        webview.setHidden(true);
      }

      #[cfg(any(debug_assertions, feature = "devtools"))]
      if attributes.devtools {
        let has_inspectable_property: bool =
          NSObject::respondsToSelector(&webview, objc2::sel!(setInspectable:));
        if has_inspectable_property {
          webview.setInspectable(true);
        }
        // this cannot be on an `else` statement, it does not work on macOS :(
        let dev = NSString::from_str("developerExtrasEnabled");
        _preference.setValue_forKey(Some(&_yes), &dev);
      }

      // Message handler
      let ipc_handler_delegate = if let Some(ipc_handler) = attributes.ipc_handler {
        let delegate = WryWebViewDelegate::new(manager.clone(), ipc_handler, mtm);
        Some(delegate)
      } else {
        None
      };

      // Document title changed handler
      let document_title_changed_observer =
        if let Some(handler) = attributes.document_title_changed_handler {
          let delegate = DocumentTitleChangedObserver::new(webview.clone(), handler);
          Some(delegate)
        } else {
          None
        };

      let pending_scripts = Arc::new(Mutex::new(Some(Vec::new())));
      let has_download_handler = attributes.download_started_handler.is_some();
      // Download handler
      let download_delegate = if attributes.download_started_handler.is_some()
        || attributes.download_completed_handler.is_some()
      {
        let delegate = WryDownloadDelegate::new(
          attributes.download_started_handler,
          attributes.download_completed_handler,
          mtm,
        );
        Some(delegate)
      } else {
        None
      };

      let navigation_policy_delegate = WryNavigationDelegate::new(
        webview.clone(),
        pending_scripts.clone(),
        has_download_handler,
        attributes.navigation_handler,
        attributes.new_window_req_handler,
        download_delegate.clone(),
        attributes.on_page_load_handler,
        mtm,
      );

      let proto_navigation_policy_delegate =
        ProtocolObject::from_ref(navigation_policy_delegate.as_ref());
      webview.setNavigationDelegate(Some(proto_navigation_policy_delegate));

      let ui_delegate: Retained<WryWebViewUIDelegate> = WryWebViewUIDelegate::new(mtm);
      let proto_ui_delegate = ProtocolObject::from_ref(ui_delegate.as_ref());
      webview.setUIDelegate(Some(proto_ui_delegate));

      // ns window is required for the print operation
      #[cfg(target_os = "macos")]
      {
        let ns_window = ns_view.window().unwrap();
        let can_set_titlebar_style =
          ns_window.respondsToSelector(objc2::sel!(setTitlebarSeparatorStyle:));
        if can_set_titlebar_style {
          ns_window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
        }
      }

      let w = Self {
        webview: webview.clone(),
        manager: manager.clone(),
        pending_scripts,
        ipc_handler_delegate,
        document_title_changed_observer,
        navigation_policy_delegate,
        download_delegate,
        ui_delegate,
        protocol_ptrs,
        is_child,
        webview_id,
      };

      // Initialize scripts
      w.init(
r#"Object.defineProperty(window, 'ipc', {
  value: Object.freeze({postMessage: function(s) {window.webkit.messageHandlers.ipc.postMessage(s);}})
});"#,
      );
      for js in attributes.initialization_scripts {
        w.init(&js);
      }

      // Set user agent
      if let Some(user_agent) = attributes.user_agent {
        w.set_user_agent(user_agent.as_str())
      }

      // Navigation
      if let Some(url) = attributes.url {
        w.navigate_to_url(url.as_str(), attributes.headers)?;
      } else if let Some(html) = attributes.html {
        w.navigate_to_string(&html);
      }

      // Inject the web view into the window as main content
      #[cfg(target_os = "macos")]
      {
        if is_child {
          ns_view.addSubview(&webview);
        } else {
          let parent_view = WryWebViewParent::new(mtm);
          parent_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewHeightSizable
              | NSAutoresizingMaskOptions::NSViewWidthSizable,
          );
          parent_view.addSubview(&webview.clone());

          // inject the webview into the window
          let ns_window = ns_view.window().unwrap();
          // Tell the webview receive keyboard events in the window.
          // See https://github.com/tauri-apps/wry/issues/739
          ns_window.setContentView(Some(&parent_view));
          ns_window.makeFirstResponder(Some(&webview));
        }

        // make sure the window is always on top when we create a new webview
        let app = NSApplication::sharedApplication(mtm);
        NSApplication::activate(&app);
      }

      #[cfg(target_os = "ios")]
      {
        ns_view.addSubview(&webview);
      }

      Ok(w)
    }
  }

  pub fn url(&self) -> crate::Result<String> {
    url_from_webview(&self.webview)
  }

  pub fn eval(&self, js: &str, callback: Option<impl Fn(String) + Send + 'static>) -> Result<()> {
    if let Some(scripts) = &mut *self.pending_scripts.lock().unwrap() {
      scripts.push(js.into());
    } else {
      // Safety: objc runtime calls are unsafe
      unsafe {
        #[cfg(feature = "tracing")]
        let span = Mutex::new(Some(tracing::debug_span!("wry::eval").entered()));

        // we need to check if the callback exists outside the handler otherwise it's a segfault
        if let Some(callback) = callback {
          let handler = block2::RcBlock::new(move |val: *mut AnyObject, _err: *mut NSError| {
            #[cfg(feature = "tracing")]
            span.lock().unwrap().take();

            let mut result = String::new();

            if !val.is_null() {
              let json_ns_data = NSJSONSerialization::dataWithJSONObject_options_error(
                &*val,
                objc2_foundation::NSJSONWritingOptions::NSJSONWritingFragmentsAllowed,
              )
              .unwrap();
              let json_string = NSString::alloc();
              let json_string =
                NSString::initWithData_encoding(json_string, &json_ns_data, NSUTF8StringEncoding)
                  .unwrap();

              result = json_string.to_string();
            }

            callback(result);
          })
          .copy();

          self
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(js), Some(&handler));
        } else {
          #[cfg(feature = "tracing")]
          let handler = Some(
            block2::RcBlock::new(move |_val: *mut AnyObject, _err: *mut NSError| {
              span.lock().unwrap().take();
            })
            .copy(),
          );
          #[cfg(not(feature = "tracing"))]
          let handler: Option<block2::RcBlock<dyn Fn(*mut AnyObject, *mut NSError)>> = None;

          self
            .webview
            .evaluateJavaScript_completionHandler(&NSString::from_str(js), handler.as_deref());
        }
      }
    }

    Ok(())
  }

  fn init(&self, js: &str) {
    // Safety: objc runtime calls are unsafe
    unsafe {
      let userscript = WKUserScript::alloc();
      // TODO: feature to allow injecting into subframes
      let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
        userscript,
        &NSString::from_str(js),
        WKUserScriptInjectionTime::AtDocumentStart,
        true,
      );
      self.manager.addUserScript(&script);
    }
  }

  pub fn load_url(&self, url: &str) -> crate::Result<()> {
    self.navigate_to_url(url, None)
  }

  pub fn load_url_with_headers(&self, url: &str, headers: http::HeaderMap) -> crate::Result<()> {
    self.navigate_to_url(url, Some(headers))
  }

  pub fn clear_all_browsing_data(&self) -> Result<()> {
    unsafe {
      let config = self.webview.configuration();
      let store = config.websiteDataStore();
      let all_data_types = WKWebsiteDataStore::allWebsiteDataTypes();
      let date = NSDate::dateWithTimeIntervalSince1970(0.0);
      let handler = block2::RcBlock::new(|| {});
      store.removeDataOfTypes_modifiedSince_completionHandler(&all_data_types, &date, &handler);
    }
    Ok(())
  }

  fn navigate_to_url(&self, url: &str, headers: Option<http::HeaderMap>) -> crate::Result<()> {
    // Safety: objc runtime calls are unsafe
    unsafe {
      let url = NSURL::URLWithString(&NSString::from_str(url)).unwrap();
      let mut request = NSMutableURLRequest::requestWithURL(&url);
      if let Some(headers) = headers {
        for (name, value) in headers.iter() {
          let key = NSString::from_str(name.as_str());
          let value = NSString::from_str(value.to_str().unwrap_or_default());
          request.addValue_forHTTPHeaderField(&value, &key);
        }
      }
      self.webview.loadRequest(&request);
    }

    Ok(())
  }

  fn navigate_to_string(&self, html: &str) {
    // Safety: objc runtime calls are unsafe
    unsafe {
      self
        .webview
        .loadHTMLString_baseURL(&NSString::from_str(html), None);
    }
  }

  fn set_user_agent(&self, user_agent: &str) {
    unsafe {
      self
        .webview
        .setCustomUserAgent(Some(&NSString::from_str(user_agent)));
    }
  }

  pub fn print(&self) -> crate::Result<()> {
    self.print_with_options(&PrintOptions::default())
  }

  pub fn print_with_options(&self, _options: &PrintOptions) -> crate::Result<()> {
    // Safety: objc runtime calls are unsafe
    #[cfg(target_os = "macos")]
    unsafe {
      let can_print = self
        .webview
        .respondsToSelector(objc2::sel!(printOperationWithPrintInfo:));
      if can_print {
        // Create a shared print info
        let print_info = objc2_app_kit::NSPrintInfo::sharedPrintInfo();
        // let print_info: id = msg_send![print_info, init];
        print_info.setTopMargin(_options.margins.top.into());
        print_info.setRightMargin(_options.margins.right.into());
        print_info.setBottomMargin(_options.margins.bottom.into());
        print_info.setLeftMargin(_options.margins.left.into());

        // Create new print operation from the webview content
        let print_operation = self.webview.printOperationWithPrintInfo(&print_info);

        // Allow the modal to detach from the current thread and be non-blocker
        print_operation.setCanSpawnSeparateThread(true);

        // Launch the modal
        let window = self.webview.window().unwrap();
        print_operation.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
          &window,
          None,
          None,
          null_mut(),
        )
      }
    }

    Ok(())
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn open_devtools(&self) {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: Retained<AnyObject> = objc2::msg_send_id![&self.webview, _inspector];
      let () = objc2::msg_send![&tool, show];
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn close_devtools(&self) {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: Retained<AnyObject> = objc2::msg_send_id![&self.webview, _inspector];
      let () = objc2::msg_send![&tool, close];
    }
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  pub fn is_devtools_open(&self) -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
      // taken from <https://github.com/WebKit/WebKit/blob/784f93cb80a386c29186c510bba910b67ce3adc1/Source/WebKit/UIProcess/API/Cocoa/WKWebView.mm#L1939>
      let tool: Retained<AnyObject> = objc2::msg_send_id![&self.webview, _inspector];
      let is_visible: bool = objc2::msg_send![&tool, isVisible];
      is_visible
    }
    #[cfg(not(target_os = "macos"))]
    false
  }

  pub fn zoom(&self, scale_factor: f64) -> crate::Result<()> {
    unsafe {
      self.webview.setPageZoom(scale_factor);
    }

    Ok(())
  }

  pub fn set_background_color(&self, _background_color: RGBA) -> Result<()> {
    Ok(())
  }

  pub fn bounds(&self) -> crate::Result<Rect> {
    unsafe {
      let parent = self.webview.superview().unwrap();
      let parent_frame = parent.frame();
      let webview_frame = self.webview.frame();

      Ok(Rect {
        position: LogicalPosition::new(
          webview_frame.origin.x,
          parent_frame.size.height - webview_frame.origin.y - webview_frame.size.height,
        )
        .into(),
        size: LogicalSize::new(webview_frame.size.width, webview_frame.size.height).into(),
      })
    }
  }

  pub fn set_bounds(&self, #[allow(unused)] bounds: Rect) -> crate::Result<()> {
    #[cfg(target_os = "macos")]
    if self.is_child {
      let window = self.webview.window().unwrap();
      let scale_factor = window.backingScaleFactor();
      let (x, y) = bounds.position.to_logical::<f64>(scale_factor).into();
      let (width, height) = bounds.size.to_logical::<i32>(scale_factor).into();

      unsafe {
        let parent_view = self.webview.superview().unwrap();
        let frame = CGRect {
          origin: window_position(&parent_view, x, y, height),
          size: CGSize::new(width, height),
        };
        self.webview.setFrame(frame);
      }
    }

    Ok(())
  }

  pub fn set_visible(&self, visible: bool) -> Result<()> {
    self.webview.setHidden(!visible);
    Ok(())
  }

  pub fn focus(&self) -> Result<()> {
    let window = self.webview.window().unwrap();
    window.makeFirstResponder(Some(&self.webview));
    Ok(())
  }

  #[cfg(target_os = "macos")]
  pub(crate) fn reparent(&self, window: *mut NSWindow) -> crate::Result<()> {
    unsafe {
      let content_view = (*window).contentView().unwrap();
      content_view.addSubview(&self.webview);
    }

    Ok(())
  }
}

pub fn url_from_webview(webview: &WKWebView) -> Result<String> {
  let url_obj = unsafe { webview.URL().unwrap() };
  let absolute_url = unsafe { url_obj.absoluteString().unwrap() };

  let bytes = {
    let bytes: *const c_char = absolute_url.UTF8String();
    bytes as *const u8
  };

  // 4 represents utf8 encoding
  let len = absolute_url.lengthOfBytesUsingEncoding(4);
  let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };

  std::str::from_utf8(bytes)
    .map(Into::into)
    .map_err(Into::into)
}

pub fn platform_webview_version() -> Result<String> {
  unsafe {
    let bundle = NSBundle::bundleWithIdentifier(&NSString::from_str("com.apple.WebKit")).unwrap();
    let dict = bundle.infoDictionary().unwrap();
    let webkit_version = dict
      .objectForKey(&NSString::from_str("CFBundleVersion"))
      .unwrap();
    let webkit_version = Retained::cast::<NSString>(webkit_version);

    bundle.unload();
    Ok(webkit_version.to_string())
  }
}

impl Drop for InnerWebView {
  fn drop(&mut self) {
    WEBVIEW_IDS.lock().unwrap().remove(&self.webview_id);

    // We need to drop handler closures here
    unsafe {
      if let Some(ipc_handler) = self.ipc_handler_delegate.take() {
        let ipc = NSString::from_str(IPC_MESSAGE_HANDLER_NAME);
        // this will decrease the retain count of the ipc handler and trigger the drop
        ipc_handler
          .ivars()
          .controller
          .removeScriptMessageHandlerForName(&ipc);
      }

      for ptr in self.protocol_ptrs.iter() {
        if !ptr.is_null() {
          drop(Box::from_raw(*ptr));
        }
      }

      // Remove webview from window's NSView before dropping.
      self.webview.removeFromSuperview();
      self.webview.retain();
      self.manager.retain();
    }
  }
}

/// Converts from wry screen-coordinates to macOS screen-coordinates.
/// wry: top-left is (0, 0) and y increasing downwards
/// macOS: bottom-left is (0, 0) and y increasing upwards
unsafe fn window_position(view: &NSView, x: i32, y: i32, height: f64) -> CGPoint {
  let frame: CGRect = view.frame();
  CGPoint::new(x as f64, frame.size.height - y as f64 - height)
}
