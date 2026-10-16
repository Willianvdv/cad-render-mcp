//! Template library: reusable render setups (views, layout, style, size...) that work on any
//! model, so every project and every iteration is shown the same way. Built-in templates ship in
//! the binary; your own are JSON files in a template folder:
//!
//! 1. `cad-render-templates/` next to the project file (share them with the model),
//! 2. every folder in `$CAD_RENDER_TEMPLATES` (colon-separated),
//! 3. `~/.config/cad-render-mcp/templates/` (your personal library),
//! 4. the built-ins.
//!
//! The first folder that has a name wins, so a file can replace a built-in. A template file
//! `<name>.json` holds `{"description": "...", "render": {render options}}`.

use crate::server::RenderParams;
use std::path::{Path, PathBuf};

pub struct Template {
    pub name: String,
    pub description: String,
    /// Render options, without nulls.
    pub params: serde_json::Value,
    /// "built-in" or the file it came from.
    pub source: String,
}

const BUILTIN: &[(&str, &str)] = &[
    ("overview", include_str!("../templates/overview.json")),
    (
        "drawing_first_angle",
        include_str!("../templates/drawing_first_angle.json"),
    ),
    (
        "drawing_third_angle",
        include_str!("../templates/drawing_third_angle.json"),
    ),
    (
        "six_views_first_angle",
        include_str!("../templates/six_views_first_angle.json"),
    ),
    (
        "six_views_third_angle",
        include_str!("../templates/six_views_third_angle.json"),
    ),
    ("turnaround", include_str!("../templates/turnaround.json")),
    (
        "presentation",
        include_str!("../templates/presentation.json"),
    ),
];

/// Your personal template folder.
pub fn user_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/cad-render-mcp/templates"))
}

/// Template folders, highest priority first.
pub fn dirs(project_file: Option<&Path>) -> Vec<PathBuf> {
    let mut d = Vec::new();
    if let Some(dir) = project_file.and_then(Path::parent) {
        d.push(dir.join("cad-render-templates"));
    }
    if let Some(env) = std::env::var_os("CAD_RENDER_TEMPLATES") {
        d.extend(std::env::split_paths(&env).filter(|p| !p.as_os_str().is_empty()));
    }
    d.extend(user_dir());
    d
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse and check one template; unknown render options are an error, so typos don't pass silently.
fn parse(name: &str, text: &str, source: String) -> anyhow::Result<Template> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| anyhow::anyhow!("{source}: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{source}: expected a JSON object"))?;
    let mut params = crate::project::strip_nulls(
        obj.get("render")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{source}: missing \"render\": {{...}} options"))?,
    );
    let m = params
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{source}: \"render\" must be an object"))?;
    for k in ["template", "preset", "save"] {
        anyhow::ensure!(
            m.remove(k).is_none(),
            "{source}: \"{k}\" is not allowed in a template"
        );
    }
    let typed: RenderParams = serde_json::from_value(params.clone())
        .map_err(|e| anyhow::anyhow!("{source}: render options: {e}"))?;
    let known = crate::project::strip_nulls(serde_json::to_value(&typed)?);
    let unknown: Vec<&String> = params
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
        .filter(|k| known.get(k.as_str()).is_none())
        .collect();
    anyhow::ensure!(
        unknown.is_empty(),
        "{source}: unknown render option(s): {}",
        unknown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(Template {
        name: name.to_string(),
        description: obj
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        params,
        source,
    })
}

/// Every template, highest priority first per name, plus the files that failed to load.
pub fn all(project_file: Option<&Path>) -> (Vec<Template>, Vec<String>) {
    let mut out: Vec<Template> = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs(project_file) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let Some(name) = f.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if !valid_name(&name) || out.iter().any(|t| t.name == name) {
                continue;
            }
            match std::fs::read_to_string(&f)
                .map_err(anyhow::Error::from)
                .and_then(|text| parse(&name, &text, f.display().to_string()))
            {
                Ok(t) => out.push(t),
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    for (name, text) in BUILTIN {
        if !out.iter().any(|t| t.name == *name) {
            out.push(parse(name, text, "built-in".into()).expect("built-in template"));
        }
    }
    (out, errors)
}

pub fn get(project_file: Option<&Path>, name: &str) -> anyhow::Result<Template> {
    let (mut all, errors) = all(project_file);
    if let Some(i) = all.iter().position(|t| t.name == name) {
        return Ok(all.swap_remove(i));
    }
    let mut names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
    names.sort();
    anyhow::bail!(
        "no template {name:?}; available: {}{}",
        names.join(", "),
        if errors.is_empty() {
            String::new()
        } else {
            format!("\nfailed to load: {}", errors.join("; "))
        }
    )
}

/// Write a template file; returns its path.
pub fn save(
    dir: &Path,
    name: &str,
    description: &str,
    params: serde_json::Value,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        valid_name(name),
        "template names use letters, digits, _ and - (max 64)"
    );
    let mut params = crate::project::strip_nulls(params);
    if let Some(m) = params.as_object_mut() {
        for k in ["template", "preset", "save", "cameras"] {
            m.remove(k);
        }
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.json"));
    let doc = serde_json::json!({"description": description, "render": params});
    let text = serde_json::to_string_pretty(&doc)? + "\n";
    parse(name, &text, path.display().to_string())?;
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        for (name, text) in BUILTIN {
            let t = parse(name, text, "built-in".into()).unwrap();
            assert!(!t.description.is_empty(), "{name} needs a description");
        }
    }

    #[test]
    fn typos_are_errors() {
        let e = parse("x", r#"{"render": {"veiws": ["iso"]}}"#, "x.json".into())
            .err()
            .unwrap();
        assert!(e.to_string().contains("veiws"), "{e}");
        assert!(parse("x", r#"{"render": {"preset": "a"}}"#, "x.json".into()).is_err());
        assert!(!valid_name("../x") && valid_name("my_view-2"));
    }
}
