use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::ipc::Axis;
use crate::layout::{PaneId, TiledNode};
use crate::session::PaneSpawn;

pub const MAX_LAYOUT_TABS: usize = 16;
pub const MAX_LAYOUT_PANES: usize = 64;
const MAX_SPLIT_CHILDREN: usize = 16;
const MAX_COMMAND_BYTES: usize = 64 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutFile {
    tabs: Vec<LayoutTab>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LayoutTab {
    name: Option<String>,
    focus: Option<String>,
    layout: Option<LayoutNode>,
    floating: Vec<LayoutFloat>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LayoutNode {
    split: Option<LayoutSplit>,
    sizes: Vec<u32>,
    children: Vec<LayoutNode>,
    pane: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    hold: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LayoutSplit {
    Vertical,
    Horizontal,
}

impl From<LayoutSplit> for Axis {
    fn from(split: LayoutSplit) -> Self {
        match split {
            LayoutSplit::Vertical => Self::Vertical,
            LayoutSplit::Horizontal => Self::Horizontal,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LayoutFloat {
    pane: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    hold: bool,
    width_percent: u16,
    height_percent: u16,
    pinned: bool,
}

impl Default for LayoutFloat {
    fn default() -> Self {
        Self {
            pane: None,
            command: None,
            cwd: None,
            hold: false,
            width_percent: 60,
            height_percent: 60,
            pinned: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutPlan {
    pub tabs: Vec<PlannedTab>,
}

#[derive(Debug, Clone)]
pub struct PlannedTab {
    pub name: Option<String>,
    pub tiled: Option<PlannedNode>,
    pub floating: Vec<PlannedFloat>,
    pub focus_slot: Option<usize>,
    pub spawns: Vec<PaneSpawn>,
}

#[derive(Debug, Clone)]
pub enum PlannedNode {
    Leaf(usize),
    Split {
        axis: Axis,
        children: Vec<PlannedNode>,
        sizes: Vec<u32>,
    },
}

impl PlannedNode {
    pub fn to_tiled(&self, slots: &[PaneId]) -> TiledNode {
        match self {
            Self::Leaf(slot) => TiledNode::leaf(slots[*slot]),
            Self::Split {
                axis,
                children,
                sizes,
            } => TiledNode::from_children(
                *axis,
                children.iter().map(|child| child.to_tiled(slots)).collect(),
                sizes,
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedFloat {
    pub slot: usize,
    pub width_percent: u16,
    pub height_percent: u16,
    pub pinned: bool,
}

impl LayoutFile {
    pub fn load(path: &Path) -> io::Result<LayoutPlan> {
        let source = fs::read_to_string(path)?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::parse(&source, path, home.as_deref())
    }

    fn parse(source: &str, path: &Path, home: Option<&Path>) -> io::Result<LayoutPlan> {
        let file: Self = toml::from_str(source)
            .map_err(|error| invalid(format!("invalid layout {}: {error}", path.display())))?;
        file.lower(home)
    }

    fn lower(self, home: Option<&Path>) -> io::Result<LayoutPlan> {
        if !(1..=MAX_LAYOUT_TABS).contains(&self.tabs.len()) {
            return Err(invalid(format!(
                "layout must contain between 1 and {MAX_LAYOUT_TABS} tabs"
            )));
        }
        let mut planned = Vec::with_capacity(self.tabs.len());
        let mut total_panes = 0_usize;
        for (tab_index, tab) in self.tabs.into_iter().enumerate() {
            let tab_number = tab_index + 1;
            if tab.layout.is_none() && tab.floating.is_empty() {
                return Err(invalid(format!("tab {tab_number} contains no panes")));
            }
            if tab
                .name
                .as_deref()
                .is_some_and(|name| name.trim().is_empty())
            {
                return Err(invalid(format!("tab {tab_number} has an empty name")));
            }
            let mut spawns = Vec::new();
            let mut labels = HashMap::new();
            let tiled = tab
                .layout
                .map(|node| lower_node(node, tab_number, home, &mut labels, &mut spawns))
                .transpose()?;
            let mut floating = Vec::with_capacity(tab.floating.len());
            for float in tab.floating {
                let label = required_label(float.pane, tab_number, "floating pane")?;
                if labels.insert(label.clone(), spawns.len()).is_some() {
                    return Err(invalid(format!(
                        "tab {tab_number} has duplicate pane label {label:?}"
                    )));
                }
                validate_command(float.command.as_deref(), tab_number, &label)?;
                if !(10..=100).contains(&float.width_percent)
                    || !(10..=100).contains(&float.height_percent)
                {
                    return Err(invalid(format!(
                        "tab {tab_number} pane {label:?} float percentages must be between 10 and 100"
                    )));
                }
                let slot = spawns.len();
                spawns.push(spawn(
                    float.command,
                    float.cwd,
                    float.hold,
                    home,
                    tab_number,
                    &label,
                )?);
                floating.push(PlannedFloat {
                    slot,
                    width_percent: float.width_percent,
                    height_percent: float.height_percent,
                    pinned: float.pinned,
                });
            }
            let focus_slot = match tab.focus {
                Some(label) => Some(*labels.get(&label).ok_or_else(|| {
                    invalid(format!(
                        "tab {tab_number} focus names missing pane label {label:?}"
                    ))
                })?),
                None => None,
            };
            total_panes = total_panes.saturating_add(spawns.len());
            if total_panes > MAX_LAYOUT_PANES {
                return Err(invalid(format!(
                    "layout exceeds the {MAX_LAYOUT_PANES}-pane limit at tab {tab_number}"
                )));
            }
            planned.push(PlannedTab {
                name: tab.name,
                tiled,
                floating,
                focus_slot,
                spawns,
            });
        }
        if total_panes == 0 {
            return Err(invalid("layout contains no panes"));
        }
        Ok(LayoutPlan { tabs: planned })
    }
}

fn lower_node(
    node: LayoutNode,
    tab: usize,
    home: Option<&Path>,
    labels: &mut HashMap<String, usize>,
    spawns: &mut Vec<PaneSpawn>,
) -> io::Result<PlannedNode> {
    let is_leaf = node.pane.is_some();
    let is_split = node.split.is_some() || !node.children.is_empty() || !node.sizes.is_empty();
    if is_leaf == is_split {
        return Err(invalid(format!(
            "tab {tab} layout node must be exactly one of pane or split"
        )));
    }
    if is_leaf {
        let label = required_label(node.pane, tab, "pane")?;
        if labels.insert(label.clone(), spawns.len()).is_some() {
            return Err(invalid(format!(
                "tab {tab} has duplicate pane label {label:?}"
            )));
        }
        validate_command(node.command.as_deref(), tab, &label)?;
        let slot = spawns.len();
        spawns.push(spawn(node.command, node.cwd, node.hold, home, tab, &label)?);
        return Ok(PlannedNode::Leaf(slot));
    }
    if node.command.is_some() || node.cwd.is_some() || node.hold {
        return Err(invalid(format!(
            "tab {tab} split node cannot have command, cwd, or hold"
        )));
    }
    let Some(split) = node.split else {
        return Err(invalid(format!("tab {tab} split node is missing split")));
    };
    if !(2..=MAX_SPLIT_CHILDREN).contains(&node.children.len()) {
        return Err(invalid(format!(
            "tab {tab} split must contain between 2 and {MAX_SPLIT_CHILDREN} children"
        )));
    }
    if !node.sizes.is_empty()
        && (node.sizes.len() != node.children.len()
            || node.sizes.iter().any(|size| !(1..=1000).contains(size)))
    {
        return Err(invalid(format!(
            "tab {tab} split sizes must match its children and be between 1 and 1000"
        )));
    }
    let sizes = if node.sizes.is_empty() {
        vec![1; node.children.len()]
    } else {
        node.sizes
    };
    let children = node
        .children
        .into_iter()
        .map(|child| lower_node(child, tab, home, labels, spawns))
        .collect::<io::Result<Vec<_>>>()?;
    Ok(PlannedNode::Split {
        axis: split.into(),
        children,
        sizes,
    })
}

fn required_label(label: Option<String>, tab: usize, kind: &str) -> io::Result<String> {
    let label = label.ok_or_else(|| invalid(format!("tab {tab} {kind} is missing pane label")))?;
    if label.trim().is_empty() {
        return Err(invalid(format!("tab {tab} {kind} has an empty pane label")));
    }
    Ok(label)
}

fn validate_command(command: Option<&str>, tab: usize, label: &str) -> io::Result<()> {
    if command.is_some_and(|command| command.len() > MAX_COMMAND_BYTES) {
        return Err(invalid(format!(
            "tab {tab} pane {label:?} command exceeds 64 KiB"
        )));
    }
    Ok(())
}

fn spawn(
    command: Option<String>,
    cwd: Option<String>,
    hold: bool,
    home: Option<&Path>,
    tab: usize,
    label: &str,
) -> io::Result<PaneSpawn> {
    Ok(PaneSpawn {
        command: command.map(OsString::from),
        cwd: cwd
            .map(|cwd| expand_home(&cwd, home, tab, label))
            .transpose()?,
        hold_on_exit: hold,
        extra_env: Vec::new(),
    })
}

fn expand_home(value: &str, home: Option<&Path>, tab: usize, label: &str) -> io::Result<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.map(|home| home.join(rest)).ok_or_else(|| {
            invalid(format!(
                "tab {tab} pane {label:?} uses ~/ but HOME is unset"
            ))
        });
    }
    Ok(PathBuf::from(value))
}

pub fn resolve_path(value: &str) -> io::Result<PathBuf> {
    let direct = PathBuf::from(value);
    let path_like = direct.is_absolute()
        || direct.components().count() > 1
        || value.contains('/')
        || value.contains('\\');
    let candidate = if direct.exists() || path_like {
        direct
    } else {
        let mut name = direct;
        if name.extension().is_none() {
            name.set_extension("toml");
        }
        crate::config::config_dir()
            .ok_or_else(|| invalid("could not resolve the vvmux config directory"))?
            .join("layouts")
            .join(name)
    };
    fs::canonicalize(candidate)
}

pub fn validate_default_name(value: &str) -> bool {
    !value.trim().is_empty()
        && !Path::new(value)
            .components()
            .any(|component| component == Component::ParentDir)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Rect;

    fn parse(source: &str) -> io::Result<LayoutPlan> {
        LayoutFile::parse(
            source,
            Path::new("fixture.toml"),
            Some(Path::new("/home/test")),
        )
    }

    #[test]
    fn nested_layout_lowers_to_slots_and_exact_weights() {
        let plan = parse(
            r#"
[[tabs]]
name = "dev"
focus = "shell"
[tabs.layout]
split = "vertical"
sizes = [30, 70]
[[tabs.layout.children]]
pane = "editor"
command = "nvim ."
cwd = "~/src/vvmux"
[[tabs.layout.children]]
split = "horizontal"
sizes = [60, 40]
[[tabs.layout.children.children]]
pane = "shell"
[[tabs.layout.children.children]]
pane = "logs"
hold = true
"#,
        )
        .unwrap();
        assert_eq!(plan.tabs.len(), 1);
        let tab = &plan.tabs[0];
        assert_eq!(tab.name.as_deref(), Some("dev"));
        assert_eq!(tab.focus_slot, Some(1));
        assert_eq!(tab.spawns.len(), 3);
        assert_eq!(
            tab.spawns[0].cwd.as_deref(),
            Some(Path::new("/home/test/src/vvmux"))
        );
        assert!(tab.spawns[2].hold_on_exit);
        let tree = tab.tiled.as_ref().unwrap().to_tiled(&[1, 2, 3]);
        let geometry = tree.geometry(Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        });
        assert_eq!(geometry[&1].width, 30);
        assert_eq!(geometry[&2].width, 70);
        assert_eq!(geometry[&3].width, 70);
        assert_eq!(geometry[&2].height + geometry[&3].height, 30);
    }

    #[test]
    fn validation_errors_name_the_problem() {
        let cases = [
            (
                "[[tabs]]\n[tabs.layout]\nsplit='vertical'\nsizes=[1]\n[[tabs.layout.children]]\npane='a'\n[[tabs.layout.children]]\npane='b'",
                "sizes",
            ),
            (
                "[[tabs]]\n[tabs.layout]\npane='a'\nsplit='vertical'",
                "exactly one",
            ),
            (
                "[[tabs]]\nunknown=true\n[[tabs.floating]]\npane='a'",
                "unknown",
            ),
            (
                "[[tabs]]\n[tabs.layout]\nsplit='vertical'\n[[tabs.layout.children]]\npane='a'\n[[tabs.layout.children]]\npane='a'",
                "duplicate",
            ),
            (
                "[[tabs]]\nfocus='missing'\n[[tabs.floating]]\npane='a'",
                "missing",
            ),
            (
                "[[tabs]]\n[tabs.layout]\nsplit='vertical'\n[[tabs.layout.children]]\npane='a'",
                "between 2",
            ),
            ("[[tabs]]", "contains no panes"),
            (
                "[[tabs]]\n[[tabs.floating]]\npane='a'\nwidth_percent=9",
                "percentages",
            ),
        ];
        for (source, expected) in cases {
            let error = parse(source).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{error:?} did not contain {expected:?}"
            );
        }
    }

    #[test]
    fn pane_and_tab_caps_are_enforced() {
        let too_many_tabs = (0..17)
            .map(|index| format!("[[tabs]]\n[[tabs.floating]]\npane='p{index}'\n"))
            .collect::<String>();
        assert!(
            parse(&too_many_tabs)
                .unwrap_err()
                .to_string()
                .contains("16 tabs")
        );

        let panes = (0..65)
            .map(|index| format!("[[tabs.floating]]\npane='p{index}'\n"))
            .collect::<String>();
        let error = parse(&format!("[[tabs]]\n{panes}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("64-pane"), "{error}");
    }

    #[test]
    fn home_expansion_and_float_defaults_are_stable() {
        let plan = parse("[[tabs]]\n[[tabs.floating]]\npane='notes'\ncwd='~/notes'").unwrap();
        let tab = &plan.tabs[0];
        assert_eq!(
            tab.spawns[0].cwd.as_deref(),
            Some(Path::new("/home/test/notes"))
        );
        assert_eq!(tab.floating[0].width_percent, 60);
        assert_eq!(tab.floating[0].height_percent, 60);
    }

    #[test]
    fn oversized_commands_are_rejected_before_any_spawn() {
        let command = "x".repeat(MAX_COMMAND_BYTES + 1);
        let source = format!(
            "[[tabs]]\n[[tabs.floating]]\npane='too-big'\ncommand={}\n",
            toml::Value::String(command)
        );
        let error = parse(&source).unwrap_err().to_string();
        assert!(error.contains("64 KiB"), "{error}");
    }
}
