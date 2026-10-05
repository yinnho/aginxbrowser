//! Per-resource-class Fetch-Metadata, split out of client/mod.rs (god-file
//! ratchet): the Chrome vocabulary of `sec-fetch-dest` / `sec-fetch-mode` /
//! default `Accept` triples and the navigation markers that ride them. A
//! parser-loaded subresource never carries the navigation's
//! `document`/`navigate` pair, and `sec-fetch-user ?1` +
//! `upgrade-insecure-requests` exist only on navigations (#203).

/// The Chrome header triple for a resource class.
pub(crate) struct FetchMetadata {
    pub(crate) dest: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) default_accept: &'static str,
}

pub(crate) fn metadata_for(rt: ResourceType) -> FetchMetadata {
    match rt {
        ResourceType::Document => FetchMetadata {
            dest: "document",
            mode: "navigate",
            default_accept: "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
        },
        ResourceType::Script => FetchMetadata {
            dest: "script",
            mode: "no-cors",
            default_accept: "*/*",
        },
        ResourceType::Stylesheet => FetchMetadata {
            dest: "style",
            mode: "no-cors",
            default_accept: "text/css,*/*;q=0.1",
        },
        ResourceType::Image => FetchMetadata {
            dest: "image",
            mode: "no-cors",
            default_accept: "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        },
        ResourceType::Fetch => FetchMetadata {
            dest: "empty",
            mode: "cors",
            default_accept: "*/*",
        },
    }
}

pub(crate) fn is_navigation(rt: ResourceType) -> bool {
    matches!(rt, ResourceType::Document)
}

/// CDP `Network.ResourceType`-shaped label for a request. Drives
/// `RequestInfo.resource_type` and the page's NetworkEvent kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Document,
    Script,
    Stylesheet,
    Image,
    Fetch,
}

impl ResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Script => "Script",
            Self::Stylesheet => "Stylesheet",
            Self::Image => "Image",
            Self::Fetch => "Fetch",
        }
    }
}
