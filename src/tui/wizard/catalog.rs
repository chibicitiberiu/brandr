//! The image catalog served by the PXE server (`/images.json`).

use bytesize::ByteSize;
use serde::Deserialize;

use crate::util::source::fetch_small;

/// One image in the catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogImage {
    pub name: String,
    pub section: String,
    pub url: String,
    pub size: u64,
    /// "hybrid", "disk", "floppy", "cd-only" or "unknown"
    pub kind: String,
    /// How to boot the written disk, e.g. "USB-HDD / HDD"
    pub boot_hint: String,
    /// Multi-disk sets (floppy installers): id shared by all members, 1-based index
    #[serde(default)]
    pub set: Option<String>,
    #[serde(default)]
    pub set_index: Option<u32>,
    #[serde(default)]
    pub set_size: Option<u32>,
    /// Other layouts the server can produce, e.g. a floppy as a USB-HDD disk
    #[serde(default)]
    pub variants: Vec<Variant>,
}

/// A server-built alternative layout of an image (see the PXE menu service).
#[derive(Debug, Clone, Deserialize)]
pub struct Variant {
    /// "usb-hdd", "usb-zip" or "uefi"
    pub method: String,
    pub url: String,
    /// unknown until the server has built it (uefi)
    #[serde(default)]
    pub size: Option<u64>,
    pub boot_hint: String,
    /// "ready", "building", "not-built" or "error"
    #[serde(default)]
    pub state: Option<String>,
    /// polled while the server builds it
    #[serde(default)]
    pub status_url: Option<String>,
}

/// Build status of a server-side variant.
#[derive(Debug, Clone, Deserialize)]
pub struct VariantStatus {
    pub state: String,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub boot_hint: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl VariantStatus {
    pub fn fetch(url: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(&fetch_small(url, 1 << 16)?)?)
    }
}

impl CatalogImage {
    pub fn kind_label(&self) -> &'static str {
        match self.kind.as_str() {
            "hybrid" => "hybrid ISO",
            "disk" => "disk image",
            "floppy" => "floppy image",
            "cd-only" => "CD-only ISO",
            _ => "unknown",
        }
    }

    pub fn size_label(&self) -> String {
        ByteSize::b(self.size).to_string()
    }

    pub fn variant(&self, method: &str) -> Option<&Variant> {
        self.variants.iter().find(|v| v.method == method)
    }

    pub fn is_set(&self) -> bool {
        self.set_size.is_some_and(|n| n > 1)
    }
}

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub images: Vec<CatalogImage>,
}

impl Catalog {
    pub fn fetch(url: &str) -> anyhow::Result<Self> {
        let body = fetch_small(url, 16 << 20)?;
        let catalog: Catalog = serde_json::from_slice(&body)?;
        if catalog.images.is_empty() {
            anyhow::bail!("the catalog at {url} lists no images");
        }
        Ok(catalog)
    }

    /// All disks of `image`'s set in order, starting at `image` itself.
    pub fn set_from(&self, image: &CatalogImage) -> Vec<CatalogImage> {
        let Some(set) = &image.set else {
            return vec![image.clone()];
        };
        let start = image.set_index.unwrap_or(1);
        let mut disks: Vec<CatalogImage> = self
            .images
            .iter()
            .filter(|i| i.set.as_ref() == Some(set) && i.set_index.unwrap_or(0) >= start)
            .cloned()
            .collect();
        disks.sort_by_key(|i| i.set_index);
        disks
    }
}
