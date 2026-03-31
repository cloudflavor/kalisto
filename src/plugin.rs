use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    pub id: String,
    pub category: String,
    pub install_kind: String,
    pub conflicts: Vec<String>,
    pub description: String,
}

pub fn built_in_catalog() -> Result<Vec<PluginSpec>> {
    // TODO: why does this assume a relative path
    parse_catalog(include_str!("../plugins/config.toml"))
}

fn parse_catalog(input: &str) -> Result<Vec<PluginSpec>> {
    let mut plugins = Vec::new();
    let mut current = ParsedPlugin::default();
    let mut in_plugin = false;

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // TODO: why's this parsed by hand, looks like a toml anchor?
        if line == "[[plugin]]" {
            if in_plugin {
                plugins.push(current.finish(line_number - 1)?);
                current = ParsedPlugin::default();
            }
            in_plugin = true;
            continue;
        }

        if !in_plugin {
            return Err(Error::InvalidArgs(format!(
                "invalid plugin catalog at line {line_number}: expected `[[plugin]]`"
            )));
        }

        let (key, value) = line.split_once('=').ok_or_else(|| {
            Error::InvalidArgs(format!(
                "invalid plugin catalog at line {line_number}: expected `key = value`"
            ))
        })?;
        let key = key.trim();
        let value = value.trim();

        match key {
            "id" => current.id = Some(parse_string(value, line_number)?),
            "category" => current.category = Some(parse_string(value, line_number)?),
            "install_kind" => current.install_kind = Some(parse_string(value, line_number)?),
            "description" => current.description = Some(parse_string(value, line_number)?),
            "conflicts" => current.conflicts = parse_string_array(value, line_number)?,
            other => {
                return Err(Error::InvalidArgs(format!(
                    "invalid plugin catalog at line {line_number}: unsupported key `{other}`"
                )));
            }
        }
    }

    if in_plugin {
        plugins.push(current.finish(input.lines().count())?);
    }

    if plugins.is_empty() {
        return Err(Error::InvalidArgs(
            "plugin catalog does not define any plugins".to_owned(),
        ));
    }

    Ok(plugins)
}

#[derive(Debug, Default)]
struct ParsedPlugin {
    id: Option<String>,
    category: Option<String>,
    install_kind: Option<String>,
    conflicts: Vec<String>,
    description: Option<String>,
}

impl ParsedPlugin {
    fn finish(self, line_number: usize) -> Result<PluginSpec> {
        Ok(PluginSpec {
            id: required_field(self.id, "id", line_number)?,
            category: required_field(self.category, "category", line_number)?,
            install_kind: required_field(self.install_kind, "install_kind", line_number)?,
            conflicts: self.conflicts,
            description: required_field(self.description, "description", line_number)?,
        })
    }
}

fn required_field(value: Option<String>, field: &str, line_number: usize) -> Result<String> {
    value.ok_or_else(|| {
        Error::InvalidArgs(format!(
            "invalid plugin catalog near line {line_number}: missing `{field}`"
        ))
    })
}

fn parse_string(value: &str, line_number: usize) -> Result<String> {
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        return Err(Error::InvalidArgs(format!(
            "invalid plugin catalog at line {line_number}: expected quoted string"
        )));
    }
    Ok(value[1..value.len() - 1].to_owned())
}

fn parse_string_array(value: &str, line_number: usize) -> Result<Vec<String>> {
    if !(value.starts_with('[') && value.ends_with(']')) {
        return Err(Error::InvalidArgs(format!(
            "invalid plugin catalog at line {line_number}: expected string array"
        )));
    }
    let inner = value[1..value.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    inner
        .split(',')
        .map(|part| parse_string(part.trim(), line_number))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{built_in_catalog, parse_catalog};

    #[test]
    fn built_in_catalog_loads_plugins() {
        let catalog = built_in_catalog().unwrap();
        assert!(!catalog.is_empty());
        assert!(catalog.iter().any(|plugin| plugin.id == "cni/cilium"));
    }

    #[test]
    fn parse_catalog_reads_string_arrays() {
        // TODO: why's this trash not toml struct serialized or struct with Default
        let catalog = parse_catalog(
            r#"
[[plugin]]
id = "cni/cilium"
category = "cni"
install_kind = "manifest"
conflicts = ["cni/flannel"]
description = "Cilium networking"
"#,
        )
        .unwrap();

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].conflicts, vec!["cni/flannel".to_owned()]);
    }
}
