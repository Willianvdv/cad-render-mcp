//! Per-project store of saved viewpoints, poses and motions (`.cad-render.json` next to the model),
//! so the same point of view can be reproduced in every session.

use crate::camera::CamSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedView {
    /// Render parameters (selectors, style, section, size...) as given to `render`.
    pub params: serde_json::Value,
    /// Frozen cameras, one per view tile: replayed exactly, even when the model changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cameras: Option<Vec<CamSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedPose {
    /// Part assembly path -> column-major 4x4 placement.
    pub parts: BTreeMap<String, [f32; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Project {
    #[serde(default)]
    pub views: BTreeMap<String, SavedView>,
    #[serde(default)]
    pub poses: BTreeMap<String, SavedPose>,
    /// Motion parameters as given to `motion`.
    #[serde(default)]
    pub motions: BTreeMap<String, serde_json::Value>,
}

impl Project {
    pub fn load(path: &Path) -> anyhow::Result<Project> {
        if !path.exists() {
            return Ok(Project::default());
        }
        let s = std::fs::read_to_string(path)?;
        serde_json::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn summary(&self) -> String {
        let names = |m: Vec<&String>| {
            if m.is_empty() {
                "-".to_string()
            } else {
                m.into_iter().cloned().collect::<Vec<_>>().join(", ")
            }
        };
        format!(
            "views: {}\nposes: {}\nmotions: {}",
            names(self.views.keys().collect()),
            names(self.poses.keys().collect()),
            names(self.motions.keys().collect())
        )
    }
}

/// Default project file: `.cad-render.json` in the directory of the first opened file (or the
/// directory itself when a directory was opened).
pub fn default_path(first: &Path) -> PathBuf {
    let dir = if first.is_dir() {
        first.to_path_buf()
    } else {
        first.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    dir.join(".cad-render.json")
}

/// Overlay the non-null fields of `over` onto `base` (both JSON objects).
pub fn overlay(base: &serde_json::Value, over: &serde_json::Value) -> serde_json::Value {
    let mut out = base.clone();
    if let (Some(o), Some(src)) = (out.as_object_mut(), over.as_object()) {
        for (k, v) in src {
            if !v.is_null() {
                o.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Drop null fields recursively (stored presets stay small and readable).
pub fn strip_nulls(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => serde_json::Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(strip_nulls).collect())
        }
        o => o,
    }
}
