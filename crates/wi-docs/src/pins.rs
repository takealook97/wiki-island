//! Checksummed page maps and page/region pin resolution.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use wi_history::hash_bytes;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageRange {
    pub page: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMap {
    pub unit: String,
    pub body_checksum: String,
    pub body_length: usize,
    pub pages: Vec<PageRange>,
}

impl PageMap {
    pub fn from_pages(pages: &[String], bytes: bool) -> (String, Self) {
        let mut body = String::new();
        let mut ranges = Vec::new();
        let mut offset = 0;
        for (i, text) in pages.iter().enumerate() {
            let start = offset;
            body.push_str(text);
            offset += if bytes {
                text.len()
            } else {
                text.chars().count()
            };
            ranges.push(PageRange {
                page: i + 1,
                start,
                end: offset,
            });
        }
        let checksum = hash_bytes(body.as_bytes());
        (
            body,
            Self {
                unit: if bytes {
                    "utf8_byte"
                } else {
                    "unicode_code_point"
                }
                .into(),
                body_checksum: checksum,
                body_length: offset,
                pages: ranges,
            },
        )
    }

    pub fn validate(&self, body: &str) -> Result<()> {
        let bytes = match self.unit.as_str() {
            "utf8_byte" => true,
            "unicode_code_point" => false,
            _ => return Err(Error::Invalid("unknown page-map unit".into())),
        };
        let length = if bytes {
            body.len()
        } else {
            body.chars().count()
        };
        if self.body_checksum != hash_bytes(body.as_bytes()) || self.body_length != length {
            return Err(Error::Invalid("body does not match page map".into()));
        }
        let mut offset = 0;
        for (i, page) in self.pages.iter().enumerate() {
            if page.page != i + 1
                || page.start != offset
                || page.end < page.start
                || page.end > length
                || (bytes
                    && (!body.is_char_boundary(page.start) || !body.is_char_boundary(page.end)))
            {
                return Err(Error::Invalid(
                    "page map is not a consecutive partition".into(),
                ));
            }
            offset = page.end;
        }
        if offset != length {
            return Err(Error::Invalid("page map does not cover body".into()));
        }
        Ok(())
    }
}

/// A top-left rectangle, represented as [left, top, right, bottom].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub bbox: [f64; 4],
    pub unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageGeometry {
    pub width: f64,
    pub height: f64,
    pub unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PagePin {
    pub n: usize,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionPin {
    pub page: usize,
    pub bbox: [f64; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPin {
    pub page: usize,
    pub label: Option<String>,
    pub bytes: PageRange,
    pub region: Option<Region>,
}

/// Resolve against the exact extraction body. Region units come from recorded
/// source geometry, never a caller-supplied guess.
pub fn resolve_page(body: &str, map: &PageMap, pin: &PagePin) -> Result<ResolvedPin> {
    map.validate(body)?;
    if pin
        .label
        .as_ref()
        .is_some_and(|s| s.trim().is_empty() || s.contains(['\r', '\n']))
    {
        return Err(Error::Invalid(
            "page label must be nonempty and single-line".into(),
        ));
    }
    if map.unit != "utf8_byte" {
        return Err(Error::Invalid("pins require the UTF-8 byte map".into()));
    }
    let range = map
        .pages
        .iter()
        .find(|p| p.page == pin.n)
        .cloned()
        .ok_or_else(|| Error::Invalid("page does not exist".into()))?;
    Ok(ResolvedPin {
        page: pin.n,
        label: pin.label.clone(),
        bytes: range,
        region: None,
    })
}

pub fn resolve_region(
    body: &str,
    map: &PageMap,
    geometry: &[Option<PageGeometry>],
    pin: &RegionPin,
) -> Result<ResolvedPin> {
    let mut resolved = resolve_page(
        body,
        map,
        &PagePin {
            n: pin.page,
            label: None,
        },
    )?;
    let bounds = geometry
        .get(pin.page - 1)
        .and_then(Option::as_ref)
        .ok_or_else(|| Error::Invalid("region geometry unavailable for this format/page".into()))?;
    if ![bounds.width, bounds.height]
        .iter()
        .all(|n| n.is_finite() && *n > 0.0)
        || !matches!(bounds.unit.as_str(), "pixel" | "pdf_point")
    {
        return Err(Error::Invalid("invalid recorded page geometry".into()));
    }
    let [left, top, right, bottom] = pin.bbox;
    if !pin.bbox.iter().all(|n| n.is_finite())
        || left < 0.0
        || top < 0.0
        || right <= left
        || bottom <= top
        || right > bounds.width
        || bottom > bounds.height
    {
        return Err(Error::Invalid("bbox is outside page geometry".into()));
    }
    resolved.region = Some(Region {
        bbox: pin.bbox,
        unit: bounds.unit.clone(),
    });
    Ok(resolved)
}
