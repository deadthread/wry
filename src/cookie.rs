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