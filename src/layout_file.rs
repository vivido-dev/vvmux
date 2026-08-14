use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ipc::Axis;
use crate::layout::{PaneId, TiledNode};
use crate::session::PaneSpawn;

pub const MAX_LAYOUT_TABS: usize = 16;
pub const MAX_LAYOUT_PANES: usize = 64;
const MAX_SPLIT_CHILDREN: usize = 16;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// The conventional startup layout, applied when a session is created without `--layout`.
pub const STARTUP_FILE: &str = "startup.toml";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutFile {
    tabs: Vec<LayoutTab>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutTab {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    focus: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    layout: Option<LayoutNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    floating: Vec<LayoutFloat>,
}

/// A layout node is a leaf or a split, never both; the constructors are what keep that true on
/// the writing side, exactly as `lower_node` enforces it on the reading side.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    split: Option<LayoutSplit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sizes: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    hold: bool,
    /// Named for the non-default state so a transparent pane — what every pane is unless the user
    /// says otherwise — keeps serializing exactly as it did before this key existed.
    #[serde(skip_serializing_if = "is_false")]
    opaque: bool,
    // Arrays of tables must follow every scalar and inline value in the same table, so `children`
    // stays last: TOML serialization order is part of this struct's contract.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<LayoutNode>,
}

#[derive(Debug, Deserialize, Serialize)]
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

impl From<Axis> for LayoutSplit {
    fn from(axis: Axis) -> Self {
        match axis {
            Axis::Vertical => Self::Vertical,
            Axis::Horizontal => Self::Horizontal,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutFloat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    hold: bool,
    width_percent: u16,
    height_percent: u16,
    #[serde(skip_serializing_if = "is_false")]
    pinned: bool,
    /// Named for the non-default state, exactly as on `LayoutNode`.
    #[serde(skip_serializing_if = "is_false")]
    opaque: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
            opaque: false,
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
                    float.opaque,
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

impl LayoutNode {
    pub fn leaf(pane: String, cwd: Option<String>, opaque: bool) -> Self {
        Self {
            pane: Some(pane),
            cwd,
            opaque,
            ..Self::default()
        }
    }

    pub fn split(axis: Axis, sizes: Vec<u32>, children: Vec<LayoutNode>) -> Self {
        Self {
            split: Some(axis.into()),
            sizes,
            children,
            ..Self::default()
        }
    }
}

impl LayoutFloat {
    pub fn new(
        pane: String,
        cwd: Option<String>,
        width_percent: u16,
        height_percent: u16,
        pinned: bool,
        opaque: bool,
    ) -> Self {
        Self {
            pane: Some(pane),
            command: None,
            cwd,
            hold: false,
            width_percent,
            height_percent,
            pinned,
            opaque,
        }
    }
}

impl LayoutNode {
    fn pane_count(&self) -> usize {
        if self.pane.is_some() {
            1
        } else {
            self.children.iter().map(Self::pane_count).sum()
        }
    }
}

impl LayoutTab {
    fn pane_count(&self) -> usize {
        self.layout.as_ref().map_or(0, LayoutNode::pane_count) + self.floating.len()
    }

    pub fn new(
        name: Option<String>,
        focus: Option<String>,
        layout: Option<LayoutNode>,
        floating: Vec<LayoutFloat>,
    ) -> Self {
        Self {
            name,
            focus,
            layout,
            floating,
        }
    }
}

impl LayoutFile {
    pub fn from_tabs(tabs: Vec<LayoutTab>) -> Self {
        Self { tabs }
    }

    /// Tab and pane counts, so a save can report what it wrote.
    pub fn counts(&self) -> (usize, usize) {
        let panes = self.tabs.iter().map(LayoutTab::pane_count).sum();
        (self.tabs.len(), panes)
    }

    /// Render this layout as TOML, proving it loads before the caller writes it anywhere.
    ///
    /// The captured session and the parser share these structs, so the round trip is what turns a
    /// capture bug into an error at save time instead of a session that will not start.
    pub fn render(&self) -> io::Result<String> {
        let rendered = toml::to_string_pretty(self)
            .map_err(|error| invalid(format!("could not render layout: {error}")))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::parse(&rendered, Path::new("<generated>"), home.as_deref())?;
        Ok(rendered)
    }
}

/// The conventional startup layout path, when the file exists.
///
/// Canonicalized because the daemon re-reads it after detaching from the caller's directory.
pub fn startup_path() -> Option<PathBuf> {
    fs::canonicalize(crate::config::config_dir()?.join(STARTUP_FILE)).ok()
}

/// Resolve a save target the user typed.
///
/// A bare name lands in the config directory and gains a `.toml` extension; anything path-like is
/// used as written, with `~/` expanded. The file need not exist yet, so this never canonicalizes.
pub fn resolve_save_path(value: &str) -> io::Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid("layout name is empty"));
    }
    if !validate_default_name(value) {
        return Err(invalid("layout path must not contain .."));
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| invalid("layout path uses ~/ but HOME is unset"))?;
        return Ok(home.join(rest));
    }
    let direct = PathBuf::from(value);
    if direct.is_absolute() || value.contains('/') || value.contains('\\') {
        return Ok(direct);
    }
    let mut name = direct;
    if name.extension().is_none() {
        name.set_extension("toml");
    }
    Ok(crate::config::config_dir()
        .ok_or_else(|| invalid("could not resolve the vvmux config directory"))?
        .join(name))
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
        spawns.push(spawn(
            node.command,
            node.cwd,
            node.hold,
            node.opaque,
            home,
            tab,
            &label,
        )?);
        return Ok(PlannedNode::Leaf(slot));
    }
    if node.command.is_some() || node.cwd.is_some() || node.hold || node.opaque {
        return Err(invalid(format!(
            "tab {tab} split node cannot have command, cwd, hold, or opaque"
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
    opaque: bool,
    home: Option<&Path>,
    tab: usize,
    label: &str,
) -> io::Result<PaneSpawn> {
    Ok(PaneSpawn {
        command: command.map(OsString::from),
        argv: None,
        cwd: cwd
            .map(|cwd| expand_home(&cwd, home, tab, label))
            .transpose()?,
        // A saved layout is authoritative about the pane it recorded, so it pins the state rather
        // than falling through to the config default.
        transparent: Some(!opaque),
        hold_on_exit: hold,
        extra_env: Vec::new(),
        role: crate::session::PaneRole::Core,
        vivid_capability: true,
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

    /// The writer and the parser share one schema, so a captured layout must load back with the
    /// same weights, focus, and float geometry it was built from.
    #[test]
    fn rendered_layout_round_trips_through_the_parser() {
        let file = LayoutFile::from_tabs(vec![
            LayoutTab::new(
                Some("dev".to_owned()),
                Some("p2".to_owned()),
                Some(LayoutNode::split(
                    Axis::Vertical,
                    vec![30, 70],
                    vec![
                        LayoutNode::leaf("p1".to_owned(), Some("~/src/vvmux".to_owned()), true),
                        LayoutNode::leaf("p2".to_owned(), None, false),
                    ],
                )),
                Vec::new(),
            ),
            LayoutTab::new(
                None,
                Some("p2".to_owned()),
                Some(LayoutNode::split(
                    Axis::Horizontal,
                    vec![1, 3],
                    vec![
                        LayoutNode::leaf("p1".to_owned(), None, false),
                        LayoutNode::leaf("p2".to_owned(), None, false),
                    ],
                )),
                vec![LayoutFloat::new("p3".to_owned(), None, 40, 50, true, true)],
            ),
        ]);
        let rendered = file.render().unwrap();
        // A capture never replays a command, and the absent-by-default fields stay out of the file.
        assert!(!rendered.contains("command"), "{rendered}");
        assert!(!rendered.contains("hold"), "{rendered}");

        let plan = parse(&rendered).unwrap();
        assert_eq!(plan.tabs.len(), 2);
        let first = &plan.tabs[0];
        assert_eq!(first.name.as_deref(), Some("dev"));
        assert_eq!(first.focus_slot, Some(1));
        assert_eq!(
            first.spawns[0].cwd.as_deref(),
            Some(Path::new("/home/test/src/vvmux"))
        );
        // A saved layout pins what it recorded rather than deferring to `[panes].transparent`, so
        // reopening it reproduces the pane the user actually saved.
        assert_eq!(first.spawns[0].transparent, Some(false));
        assert_eq!(first.spawns[1].transparent, Some(true));
        let geometry = first
            .tiled
            .as_ref()
            .unwrap()
            .to_tiled(&[1, 2])
            .geometry(Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 30,
            });
        assert_eq!(geometry[&1].width, 30);
        assert_eq!(geometry[&2].width, 70);

        let second = &plan.tabs[1];
        assert_eq!(second.spawns.len(), 3);
        assert_eq!(second.floating[0].slot, 2);
        assert_eq!(second.floating[0].width_percent, 40);
        assert_eq!(second.floating[0].height_percent, 50);
        assert!(second.floating[0].pinned);
        assert_eq!(second.spawns[2].transparent, Some(false));
        let geometry = second
            .tiled
            .as_ref()
            .unwrap()
            .to_tiled(&[1, 2])
            .geometry(Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 40,
            });
        assert_eq!(geometry[&1].height, 10);
        assert_eq!(geometry[&2].height, 30);
    }

    /// `opaque` arrived after layouts were already in use, so a file written before it existed
    /// must still load, and must load as the transparent pane it described.
    #[test]
    fn a_layout_without_opaque_loads_as_transparent() {
        let plan = parse(
            r#"
[[tabs]]
[tabs.layout]
pane = "p1"
"#,
        )
        .unwrap();

        assert_eq!(plan.tabs[0].spawns[0].transparent, Some(true));
    }

    /// `opaque` describes a pane, so it has no meaning on a split — the same rule the other
    /// leaf-only keys already follow.
    #[test]
    fn a_split_cannot_be_opaque() {
        let error = parse(
            r#"
[[tabs]]
[tabs.layout]
split = "vertical"
opaque = true

[[tabs.layout.children]]
pane = "p1"

[[tabs.layout.children]]
pane = "p2"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("opaque"), "{error}");
    }

    /// The save prompt accepts a file name or a path; only a bare name means "in the config
    /// directory", and a parent-directory escape is refused before anything is written.
    #[test]
    fn save_targets_separate_bare_names_from_paths() {
        assert_eq!(
            resolve_save_path("/tmp/custom.toml").unwrap(),
            PathBuf::from("/tmp/custom.toml")
        );
        assert_eq!(
            resolve_save_path("nested/dev").unwrap(),
            PathBuf::from("nested/dev"),
            "a path keeps its exact spelling, extension included"
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert_eq!(
                resolve_save_path("~/layouts/dev.toml").unwrap(),
                home.join("layouts/dev.toml")
            );
        }
        for rejected in ["", "   ", "../escape.toml", "~/../escape.toml"] {
            assert!(
                resolve_save_path(rejected).is_err(),
                "{rejected:?} was accepted"
            );
        }
    }

    #[test]
    fn rendered_leaves_and_splits_stay_disjoint() {
        let file = LayoutFile::from_tabs(vec![LayoutTab::new(
            None,
            None,
            Some(LayoutNode::split(
                Axis::Vertical,
                vec![1, 1],
                vec![
                    LayoutNode::leaf("p1".to_owned(), Some("/tmp".to_owned()), false),
                    LayoutNode::leaf("p2".to_owned(), None, false),
                ],
            )),
            Vec::new(),
        )]);
        let rendered = file.render().unwrap();
        let value: toml::Value = toml::from_str(&rendered).unwrap();
        let root = &value["tabs"][0]["layout"];
        assert!(root.get("pane").is_none(), "{rendered}");
        assert!(root.get("cwd").is_none(), "{rendered}");
        let leaf = &root["children"][0];
        assert!(leaf.get("split").is_none(), "{rendered}");
        assert!(leaf.get("sizes").is_none(), "{rendered}");
        assert!(leaf.get("children").is_none(), "{rendered}");
        assert_eq!(leaf["cwd"].as_str(), Some("/tmp"));
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
