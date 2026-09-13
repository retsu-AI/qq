//! `tree`: a depth-bounded, ignore-aware directory tree with sizes and counts.
//! `list_dir` is a hidden alias for `tree depth=1` so persisted transcripts
//! and grants keep resolving.
//!
//! The tree fills breadth-first so the top level is complete before any
//! deeper level appears: a model asking about a repository sees every
//! top-level entry even when the entry budget is small.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::workspace::Workspace;

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    output::{Bounds, Header, MARKER_PREFIX},
    search::{CANCELLED_MESSAGE, path_error},
    walk::{
        EntryKind, IgnoreStack, PathFilter, ScanBudget, StopReason, list_children, relative_string,
    },
};

pub(super) const TREE_BOUNDS: Bounds = Bounds::new(16 * 1024, 4_000);
pub(super) const MAX_DEPTH: usize = 6;
const DEFAULT_DEPTH: usize = 2;
pub(super) const MAX_ENTRIES: usize = 500;
const DEFAULT_ENTRIES: usize = 120;
/// Directory entries visited across the listing and the count sub-walks.
pub(super) const MAX_SCAN_ENTRIES: usize = 20_000;
const TREE_DEADLINE: Duration = Duration::from_secs(2);
/// Entries one directory's `(<files>f <dirs>d)` sub-walk may visit.
const MAX_COUNT_ENTRIES: usize = 2_000;
/// Longest single-child chain collapsed into one row (`a/b/c/d`).
const MAX_CHAIN: usize = 4;
/// Leaf files pack onto one row up to this width.
const PACK_WIDTH: usize = 100;
pub(super) const MAX_GLOB_BYTES: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TreeArgs {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_depth")]
    depth: usize,
    #[serde(default = "default_entries")]
    limit: usize,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    include_ignored: bool,
}

/// `list_dir` arguments: the pre-T2 shape, mapped onto `tree depth=1`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDirArgs {
    path: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

impl From<ListDirArgs> for TreeArgs {
    fn from(arguments: ListDirArgs) -> Self {
        Self {
            path: arguments.path,
            depth: 1,
            limit: arguments.limit.clamp(1, MAX_ENTRIES),
            glob: None,
            include_ignored: true,
        }
    }
}

fn default_path() -> String {
    ".".to_owned()
}

const fn default_depth() -> usize {
    DEFAULT_DEPTH
}

const fn default_entries() -> usize {
    DEFAULT_ENTRIES
}

const fn default_list_limit() -> usize {
    MAX_ENTRIES
}

struct Node {
    child: super::walk::Child,
    /// Directory statistics from a bounded sub-walk.
    counts: Option<Counts>,
    /// Directory listed (children appended) rather than only shown.
    expanded: bool,
    /// Children beyond the entry budget or depth: `+N more`.
    unlisted: usize,
    children: Vec<usize>,
}

#[derive(Clone, Copy, Default)]
struct Counts {
    files: usize,
    dirs: usize,
    capped: bool,
}

pub(super) fn tree(
    workspace: &Workspace,
    arguments: TreeArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    let started = Instant::now();
    if arguments.depth == 0 || arguments.depth > MAX_DEPTH {
        return ToolOutput::error(format!(
            "invalid_depth: depth must be between 1 and {MAX_DEPTH}"
        ));
    }
    if arguments.limit == 0 || arguments.limit > MAX_ENTRIES {
        return ToolOutput::error(format!(
            "invalid_limit: limit must be between 1 and {MAX_ENTRIES}"
        ));
    }
    let filter = match &arguments.glob {
        None => PathFilter::new(&[], &[]),
        Some(glob) if glob.is_empty() || glob.len() > MAX_GLOB_BYTES => {
            return ToolOutput::error(format!(
                "bad_glob: glob must be 1 to {MAX_GLOB_BYTES} bytes"
            ));
        }
        Some(glob) => PathFilter::new(std::slice::from_ref(glob), &[]),
    };
    let filter = match filter {
        Ok(filter) => filter,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let root = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(path_error(&arguments.path, &error)),
    };
    let root = relative_string(&root);
    if !workspace.root().is_dir(&root) {
        return ToolOutput::error("not_a_directory");
    }

    let deadline = started + TREE_DEADLINE;
    let mut budget = ScanBudget::new(MAX_SCAN_ENTRIES, u64::MAX, deadline);
    let mut nodes: Vec<Node> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut root_unlisted = 0_usize;
    let mut shown = 0_usize;
    let mut stop: Option<StopReason> = None;
    // Breadth-first: (directory path, depth of its children, parent node).
    let mut queue: VecDeque<(String, usize, Option<usize>)> = VecDeque::new();
    queue.push_back((root.clone(), 1, None));
    while let Some((dir, depth, parent)) = queue.pop_front() {
        if cancelled.is_cancelled() {
            return ToolOutput::error(CANCELLED_MESSAGE);
        }
        if stop.is_some() {
            // Never listed: shown as a directory with counts only.
            if let Some(parent) = parent {
                nodes[parent].expanded = false;
            }
            continue;
        }
        // Ignore matching needs each directory's ancestor chain, so the BFS
        // opens a stack per directory rather than sharing one.
        let mut stack = IgnoreStack::open(workspace, &dir, arguments.include_ignored);
        let children = match list_children(workspace, &dir, &mut stack, &mut budget.unreadable) {
            Ok(children) => children,
            // An unreadable subdirectory shows unexpanded; the root itself
            // failing is the call's error.
            Err(error) => match parent {
                Some(parent) => {
                    nodes[parent].expanded = false;
                    continue;
                }
                None => return ToolOutput::error(error.to_string()),
            },
        };
        let mut unlisted = 0_usize;
        for child in children {
            if let Some(reason) = budget.charge_entry() {
                stop = Some(reason);
            }
            let is_dir = child.is_dir();
            // Ignored files are dropped; ignored directories appear once, at
            // the top level, as `…ignored` so the model knows they exist.
            if (!is_dir && (child.ignored || !filter.admits_file(&child.path)))
                || (is_dir && child.ignored && depth > 1)
            {
                continue;
            }
            if shown >= arguments.limit || stop.is_some() {
                unlisted += 1;
                continue;
            }
            shown += 1;
            let index = nodes.len();
            let descend = is_dir && !child.ignored && depth < arguments.depth;
            let counts = (is_dir && !child.ignored).then(|| {
                count_dir(
                    workspace,
                    &child.path,
                    arguments.include_ignored,
                    &mut budget,
                )
            });
            if descend {
                queue.push_back((child.path.clone(), depth + 1, Some(index)));
            }
            nodes.push(Node {
                child,
                counts,
                expanded: descend,
                unlisted: 0,
                children: Vec::new(),
            });
            match parent {
                Some(parent) => nodes[parent].children.push(index),
                None => roots.push(index),
            }
        }
        match parent {
            Some(parent) => nodes[parent].unlisted = unlisted,
            None => root_unlisted = unlisted,
        }
    }

    let (total_files, total_dirs) =
        nodes
            .iter()
            .fold((0, 0), |(files, dirs), node| match node.child.kind {
                EntryKind::Dir => (files, dirs + 1),
                _ => (files + 1, dirs),
            });
    let unlisted = root_unlisted + nodes.iter().map(|node| node.unlisted).sum::<usize>();
    let mut header = Header::new("tree", Some(&root))
        .field("depth", arguments.depth)
        .field("entries", format_args!("{shown}/{}", shown + unlisted))
        .field("files", total_files)
        .field("dirs", total_dirs);
    if let Some(reason) = stop {
        header = header.field("partial", reason.label());
    }
    let mut text = header.into_line();
    render_children(&nodes, &roots, 0, &mut text);
    if root_unlisted > 0 {
        let _ = writeln!(
            text,
            "{MARKER_PREFIX}{root_unlisted} more entries; raise limit]…"
        );
    }
    ToolOutput::bounded(text, &TREE_BOUNDS, false)
}

/// `(<files>f <dirs>d)` for one directory from a bounded sub-walk that skips
/// ignored directories; `capped` when the sub-walk hit its own bound.
fn count_dir(
    workspace: &Workspace,
    dir: &str,
    include_ignored: bool,
    budget: &mut ScanBudget,
) -> Counts {
    let mut counts = Counts::default();
    let mut pending = vec![dir.to_owned()];
    let mut visited = 0_usize;
    let mut unreadable = 0_usize;
    while let Some(path) = pending.pop() {
        let mut stack = IgnoreStack::open(workspace, &path, include_ignored);
        let Ok(children) = list_children(workspace, &path, &mut stack, &mut unreadable) else {
            continue;
        };
        for child in children {
            visited += 1;
            if visited > MAX_COUNT_ENTRIES || budget.charge_entry().is_some() {
                counts.capped = true;
                return counts;
            }
            match child.kind {
                EntryKind::Dir => {
                    counts.dirs += 1;
                    if !child.ignored {
                        pending.push(child.path);
                    }
                }
                EntryKind::File { .. } | EntryKind::Symlink | EntryKind::Other => {
                    if !child.ignored {
                        counts.files += 1;
                    }
                }
            }
        }
    }
    counts
}

fn render(nodes: &[Node], index: usize, indent: usize, out: &mut String) {
    let node = &nodes[index];
    push_indent(out, indent);
    match node.child.kind {
        EntryKind::Dir => {
            // Collapse single-child directory chains: `a/b/c/` on one row.
            let mut current = index;
            let mut chain = 1_usize;
            out.push_str(&nodes[current].child.name);
            out.push('/');
            while chain < MAX_CHAIN {
                let node = &nodes[current];
                if !node.expanded || node.children.len() != 1 || node.unlisted != 0 {
                    break;
                }
                let only = node.children[0];
                if !nodes[only].child.is_dir() || nodes[only].child.ignored {
                    break;
                }
                out.push_str(&nodes[only].child.name);
                out.push('/');
                current = only;
                chain += 1;
            }
            let node = &nodes[current];
            if node.child.ignored {
                out.push_str(" …ignored");
            } else if let Some(counts) = node.counts {
                let _ = write!(
                    out,
                    " ({}f {}d{})",
                    counts.files,
                    counts.dirs,
                    if counts.capped { "+" } else { "" }
                );
            }
            out.push('\n');
            if node.expanded {
                render_children(nodes, &node.children, indent + 1, out);
                if node.unlisted > 0 {
                    push_indent(out, indent + 1);
                    let _ = writeln!(out, "+{} more", node.unlisted);
                }
            }
        }
        EntryKind::File { size } => {
            out.push_str(&node.child.name);
            push_size(out, size);
            out.push('\n');
        }
        EntryKind::Symlink => {
            out.push_str(&node.child.name);
            out.push_str("@\n");
        }
        EntryKind::Other => {
            out.push_str(&node.child.name);
            out.push('\n');
        }
    }
}

/// Renders one level: subdirectories one per row, leaf files packed onto
/// rows of at most [`PACK_WIDTH`] bytes.
fn render_children(nodes: &[Node], children: &[usize], indent: usize, out: &mut String) {
    let mut packed = String::new();
    let flush = |packed: &mut String, out: &mut String| {
        if !packed.is_empty() {
            push_indent(out, indent);
            out.push_str(packed);
            out.push('\n');
            packed.clear();
        }
    };
    for &child_index in children {
        let child = &nodes[child_index];
        match child.child.kind {
            EntryKind::Dir => {
                flush(&mut packed, out);
                render(nodes, child_index, indent, out);
            }
            EntryKind::File { size } => {
                let mut cell = child.child.name.clone();
                push_size(&mut cell, size);
                if !packed.is_empty() && packed.len() + 2 + cell.len() > PACK_WIDTH {
                    flush(&mut packed, out);
                }
                if !packed.is_empty() {
                    packed.push_str("  ");
                }
                packed.push_str(&cell);
            }
            EntryKind::Symlink => {
                flush(&mut packed, out);
                push_indent(out, indent);
                out.push_str(&child.child.name);
                out.push_str("@\n");
            }
            EntryKind::Other => {
                flush(&mut packed, out);
                push_indent(out, indent);
                out.push_str(&child.child.name);
                out.push('\n');
            }
        }
    }
    flush(&mut packed, out);
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

/// ` 1.2k` style size suffix: bytes below 1 KiB are exact, otherwise one
/// decimal with `k`/`M`/`G`.
fn push_size(out: &mut String, size: u64) {
    out.push(' ');
    if size < 1024 {
        let _ = write!(out, "{size}");
        return;
    }
    let (value, unit) = if size < 1024 * 1024 {
        (size as f64 / 1024.0, 'k')
    } else if size < 1024 * 1024 * 1024 {
        (size as f64 / (1024.0 * 1024.0), 'M')
    } else {
        (size as f64 / (1024.0 * 1024.0 * 1024.0), 'G')
    };
    if value >= 10.0 {
        let _ = write!(out, "{value:.0}{unit}");
    } else {
        let _ = write!(out, "{value:.1}{unit}");
    }
}
