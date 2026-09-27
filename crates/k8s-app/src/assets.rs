//! The product's own icon assets, alongside the shared Lucide catalog.
//!
//! # Why these exist rather than a Lucide name
//!
//! The kind set is drawn here, not borrowed. A generic "cube" for a Pod and a
//! generic "blocks" for every workload kind is a *category* icon, and a category
//! icon is a filename: at 14px in a sidebar, a reader cannot tell a Deployment
//! from a ReplicaSet from a DaemonSet by shape, so the name beside it has to do
//! all the work. The twelve shapes here were drawn so the silhouette alone
//! identifies the kind, which is what makes a 14px sidebar scannable.
//!
//! The form is fixed: `duotone` — 32% fill, 1.5px stroke, 2px corners. One form
//! for both appearances, because the fill is `currentColor` at a fraction of the
//! *stroke* colour, so the same file is correct on a near-black panel and on a
//! white one. Two forms would mean checking twenty-four files instead of twelve,
//! and the second form would never be the one a reader needed at 14px.
//!
//! # Ownership
//!
//! The twelve files are hand-drawn assets and nothing in this module may "fix"
//! one by editing geometry in Rust. If a shape reads wrong, the file is redrawn.

use gpui_kit::assets::AllAssets;
use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// The asset source the app installs: the product's kind set first, then the
/// whole shared catalog.
///
/// Which kind answers for which file is [`k8s_ui::design::KIND_ICON_PATHS`] — a
/// token, because which twelve kinds get bespoke shapes is a design decision.
/// This module owns the bytes and the fall-through.
///
/// Order matters and is the whole design. The shared catalog is 1,830 Lucide
/// glyphs and it covers everything the chrome needs; the twelve kind icons are
/// the product's own and would otherwise be shadowed by a same-named Lucide
/// file. So the product's assets are consulted first and everything else falls
/// through, which means a missing product asset degrades to a Lucide glyph rather
/// than to nothing.
pub struct ProductAssets;

impl AssetSource for ProductAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(bytes) = kind_asset(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        AllAssets::new("").load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = k8s_ui::design::KIND_ICON_PATHS
            .iter()
            .map(|(_, asset)| SharedString::from(*asset))
            .chain(std::iter::once(SharedString::from(FALLBACK_PATH)))
            .filter(|asset| asset.starts_with(path))
            .collect();
        listed.extend(AllAssets::new("").list(path)?);
        listed.sort();
        listed.dedup();
        Ok(listed)
    }
}

/// The stand-in every kind outside the twelve gets: one rounded square, always
/// the same one, with the caller's letter inside it.
const FALLBACK_PATH: &str = "icons/k8s-kind-fallback.svg";

/// The bytes of one of the twelve, keyed by its asset path.
fn kind_asset(path: &str) -> Option<&'static [u8]> {
    // A `match` on the literal path rather than a runtime table: the compiler
    // checks every arm against the files below, so deleting or renaming an asset
    // is a compile error rather than an icon that quietly stops rendering.
    Some(match path {
        "icons/k8s-pod.svg" => include_bytes!("../assets/icons/k8s-pod.svg").as_slice(),
        "icons/k8s-deployment.svg" => {
            include_bytes!("../assets/icons/k8s-deployment.svg").as_slice()
        }
        "icons/k8s-statefulset.svg" => {
            include_bytes!("../assets/icons/k8s-statefulset.svg").as_slice()
        }
        "icons/k8s-replicaset.svg" => {
            include_bytes!("../assets/icons/k8s-replicaset.svg").as_slice()
        }
        "icons/k8s-daemonset.svg" => include_bytes!("../assets/icons/k8s-daemonset.svg").as_slice(),
        "icons/k8s-job.svg" => include_bytes!("../assets/icons/k8s-job.svg").as_slice(),
        "icons/k8s-cronjob.svg" => include_bytes!("../assets/icons/k8s-cronjob.svg").as_slice(),
        "icons/k8s-node.svg" => include_bytes!("../assets/icons/k8s-node.svg").as_slice(),
        "icons/k8s-service.svg" => include_bytes!("../assets/icons/k8s-service.svg").as_slice(),
        "icons/k8s-ingress.svg" => include_bytes!("../assets/icons/k8s-ingress.svg").as_slice(),
        "icons/k8s-configmap.svg" => include_bytes!("../assets/icons/k8s-configmap.svg").as_slice(),
        "icons/k8s-namespace.svg" => include_bytes!("../assets/icons/k8s-namespace.svg").as_slice(),
        "icons/k8s-kind-fallback.svg" => {
            include_bytes!("../assets/icons/k8s-kind-fallback.svg").as_slice()
        }
        _ => return None,
    })
}
