#[derive(Debug)]
pub struct Cookie {
    pub(crate) domain: String,
    pub(crate) expires: f64,
    pub(crate) is_http_only: bool,
    pub(crate) is_secure: bool,
    pub(crate) is_session: bool,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) same_site: i32,
    pub(crate) value: String
}

impl Cookie {
    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn expires(&self) -> f64 {
        self.expires
    }

    pub fn is_http_only(&self) -> bool {
        self.is_http_only
    }

    pub fn is_secure(&self) -> bool {
        self.is_secure
    }

    pub fn is_session(&self) -> bool {
        self.is_session
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn same_site(&self) -> i32 {
        self.same_site
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}