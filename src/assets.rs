//! Icons compiled into the binary.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")) as &[u8])),*]
    };
}

const ICONS: &[(&str, &[u8])] = icons![
    "arrow-left",
    "arrow-right",
    "arrow-up",
    "chart",
    "chevron-right",
    "copy",
    "drive",
    "external",
    "file",
    "folder",
    "folder-search",
    "home",
    "layers",
    "link",
    "refresh",
    "x",
    "trash",
    "alert",
];

pub fn icon(name: &str) -> SharedString {
    SharedString::from(format!("icons/{name}.svg"))
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let name = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"));
        Ok(name
            .and_then(|n| ICONS.iter().find(|(k, _)| *k == n))
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(k, _)| SharedString::from(format!("icons/{k}.svg")))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}
