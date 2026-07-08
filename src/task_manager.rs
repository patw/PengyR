//! Task/prompt-template management for Pengy.
//!
//! Tasks are local prompt macros stored as JSON at ~/.config/pengy/tasks.json.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::{fs, io};

const TASKS_FILE: &str = "tasks.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default)]
    pub template: String,
    #[serde(default = "now")]
    pub created_at: String,
    #[serde(default = "now")]
    pub updated_at: String,
}

fn new_id() -> String { uuid::Uuid::new_v4().to_string() }
fn default_title() -> String { "Untitled Task".into() }
fn now() -> String { chrono::Local::now().to_rfc3339() }

fn tasks_path() -> std::path::PathBuf {
    let mut p = crate::config::pengy_config_dir();
    p.push(TASKS_FILE);
    p
}

fn normalize(mut t: Task) -> Task {
    if t.id.is_empty() { t.id = new_id(); }
    if t.title.is_empty() { t.title = default_title(); }
    if t.created_at.is_empty() { t.created_at = now(); }
    if t.updated_at.is_empty() { t.updated_at = t.created_at.clone(); }
    t
}

pub fn load_tasks() -> Vec<Task> {
    let path = tasks_path();
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Vec<Task>>(&text) {
            Ok(tasks) => tasks.into_iter().map(normalize).collect(),
            Err(_) => { backup_corrupt_file(&path); Vec::new() }
        },
        Err(_) => Vec::new(),
    }
}

pub fn save_tasks(tasks: &[Task]) -> io::Result<()> {
    let path = tasks_path();
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    let normalized: Vec<Task> = tasks.iter().cloned().map(normalize).collect();
    let json = serde_json::to_string_pretty(&normalized)?;
    let mut tmp = path.clone();
    tmp.set_extension("tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn create_task(title: &str, template: &str) -> io::Result<Task> {
    let ts = now();
    let task = Task { id: new_id(), title: clean_title(title), template: template.into(), created_at: ts.clone(), updated_at: ts };
    let mut tasks = load_tasks();
    tasks.push(task.clone());
    save_tasks(&tasks)?;
    Ok(task)
}

pub fn update_task(id: &str, title: &str, template: &str) -> io::Result<Option<Task>> {
    let mut tasks = load_tasks();
    for task in &mut tasks {
        if task.id == id {
            task.title = clean_title(title);
            task.template = template.into();
            task.updated_at = now();
            let out = task.clone();
            save_tasks(&tasks)?;
            return Ok(Some(out));
        }
    }
    Ok(None)
}

pub fn delete_task(id: &str) -> io::Result<()> {
    let mut tasks = load_tasks();
    tasks.retain(|t| t.id != id);
    save_tasks(&tasks)
}

pub fn get_task(id: &str) -> Option<Task> { load_tasks().into_iter().find(|t| t.id == id) }

pub fn extract_placeholders(template: &str) -> Vec<String> {
    let re = regex::Regex::new(r"%([^%\r\n]+)%").unwrap();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cap in re.captures_iter(template) {
        let name = cap[1].trim().to_string();
        if !name.is_empty() && seen.insert(name.clone()) { out.push(name); }
    }
    out
}

pub fn render_template(template: &str, values: &HashMap<String, String>) -> String {
    let re = regex::Regex::new(r"%([^%\r\n]+)%").unwrap();
    re.replace_all(template, |caps: &regex::Captures| {
        let name = caps[1].trim();
        values.get(name).cloned().unwrap_or_else(|| caps[0].to_string())
    }).to_string()
}

fn clean_title(title: &str) -> String { let t = title.trim(); if t.is_empty() { default_title() } else { t.into() } }

fn backup_corrupt_file(path: &std::path::Path) {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let backup = path.with_file_name(format!("{}.corrupt-{}", path.file_name().and_then(|s| s.to_str()).unwrap_or("tasks.json"), ts));
    let _ = fs::rename(path, backup);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholders_unique_trimmed_ordered() {
        assert_eq!(extract_placeholders("A % one % B %two% C %one%"), vec!["one", "two"]);
    }
    #[test]
    fn render_unknown_left_intact() {
        let mut m = HashMap::new(); m.insert("x".into(), "Y".into());
        assert_eq!(render_template("%x% %z%", &m), "Y %z%");
    }
}
