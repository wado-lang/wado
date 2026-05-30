use std::path::PathBuf;

pub struct Template {
    prefix: String,
    suffix: String,
}

impl Template {
    pub fn parse(template: &str) -> Self {
        let (prefix, suffix) = template
            .split_once("{name}")
            .expect("template must contain {name}");
        Self {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        }
    }

    pub fn discover(&self) -> Vec<(String, PathBuf)> {
        let pattern = format!("{}*{}", self.prefix, self.suffix);
        let mut results: Vec<(String, PathBuf)> = Vec::new();
        for entry in glob::glob(&pattern).expect("invalid glob pattern") {
            let Ok(path) = entry else { continue };
            let path_str = path.to_string_lossy();
            if let Some(name) = path_str
                .strip_prefix(&self.prefix)
                .and_then(|s| s.strip_suffix(&self.suffix))
            {
                results.push((name.to_string(), path));
            }
        }
        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }
}
