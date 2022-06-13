use std::str::FromStr;

use ntex_http::{uri, HeaderMap, Method, StatusCode, Uri, Version};

use crate::frame::Headers;

#[derive(Debug, Default)]
pub struct Request {
    pub uri: Uri,
    pub method: Method,
    pub version: Version,
    pub headers: HeaderMap,
}

impl Request {
    pub fn from_headers(hdrs: Headers) -> Self {
        let (pseudo, headers) = hdrs.into_parts();

        let mut parts = uri::Parts::default();

        // scheme
        if let Some(ref s) = pseudo.scheme {
            if let Ok(s) = uri::Scheme::from_str(s) {
                parts.scheme = Some(s);
            }
        }

        // authority
        if let Some(ref s) = pseudo.authority {
            if let Ok(s) = uri::Authority::from_str(s) {
                parts.authority = Some(s);
            }
        }

        // path
        if let Some(ref s) = pseudo.path {
            if let Ok(s) = uri::PathAndQuery::from_str(s) {
                parts.path_and_query = Some(s);
            }
        }

        Request {
            headers,
            uri: Uri::from_parts(parts).unwrap_or_default(),
            method: pseudo.method.clone().unwrap_or(Method::GET),
            version: Version::HTTP_2,
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
}
