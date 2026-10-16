//! `LayoutTree` to and from its handover DTO.

use super::cell::{LayoutCell, LayoutGeometry, LayoutKind};
use super::{LayoutDirection, LayoutTree};
use crate::handover::{to_usize, HandoverError, LayoutCellV1, LayoutSplitV1, LayoutTreeV1};

/// Deeper than any real window could split; a state this deep is damaged,
/// and the recursion below must not follow it off the stack.
const MAX_LAYOUT_DEPTH: usize = 256;

impl LayoutTree {
    pub(crate) fn to_handover(&self) -> LayoutTreeV1 {
        let Self { root, leaf_map } = self;
        LayoutTreeV1 {
            root: cell_to_handover(root),
            leaf_map: leaf_map
                .as_ref()
                .map(|map| map.iter().map(|&leaf| leaf as u64).collect()),
        }
    }

    /// Rebuilds the tree for a window of `pane_count` panes. Applying a tree
    /// indexes leaves by pane position, so a tree whose leaves do not match
    /// the panes is refused here instead of panicking in the new daemon.
    pub(crate) fn from_handover(
        dto: LayoutTreeV1,
        pane_count: usize,
    ) -> Result<Self, HandoverError> {
        let LayoutTreeV1 { root, leaf_map } = dto;
        let root = cell_from_handover(root, 0)?;
        let leaves = root.leaf_count();
        if leaves != pane_count {
            return Err(HandoverError::new(format!(
                "layout has {leaves} leaves for {pane_count} panes"
            )));
        }
        let leaf_map = leaf_map
            .map(|map| {
                let map = map
                    .into_iter()
                    .map(|leaf| to_usize(leaf, "layout leaf"))
                    .collect::<Result<Vec<_>, _>>()?;
                if map.len() != pane_count || map.iter().any(|&leaf| leaf >= leaves) {
                    return Err(HandoverError::new("layout leaf map does not fit its panes"));
                }
                Ok(map)
            })
            .transpose()?;
        Ok(Self { root, leaf_map })
    }
}

fn cell_to_handover(cell: &LayoutCell) -> LayoutCellV1 {
    let LayoutCell { geometry, kind } = cell;
    let LayoutGeometry {
        width,
        height,
        x,
        y,
    } = *geometry;
    LayoutCellV1 {
        width,
        height,
        x,
        y,
        split: match kind {
            LayoutKind::Pane => None,
            LayoutKind::Split(direction, children) => Some(LayoutSplitV1 {
                left_right: match direction {
                    LayoutDirection::LeftRight => true,
                    LayoutDirection::TopBottom => false,
                },
                children: children.iter().map(cell_to_handover).collect(),
            }),
        },
    }
}

fn cell_from_handover(dto: LayoutCellV1, depth: usize) -> Result<LayoutCell, HandoverError> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err(HandoverError::new("layout tree is too deep"));
    }
    let LayoutCellV1 {
        width,
        height,
        x,
        y,
        split,
    } = dto;
    let geometry = LayoutGeometry::new(width, height, x, y);
    Ok(match split {
        None => LayoutCell::pane(geometry),
        Some(LayoutSplitV1 {
            left_right,
            children,
        }) => {
            if children.is_empty() {
                return Err(HandoverError::new("layout split has no children"));
            }
            let direction = if left_right {
                LayoutDirection::LeftRight
            } else {
                LayoutDirection::TopBottom
            };
            let children = children
                .into_iter()
                .map(|child| cell_from_handover(child, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            LayoutCell::split(direction, geometry, children)
        }
    })
}
