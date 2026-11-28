//! Normalize a pinned Manufacturing tree, then net it against one warehouse.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_PIECES: i32 = 1_000_000;
const MAX_NODES: usize = 256;
const MAX_DEPTH: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct Item {
    pub sku: String,
    pub revision: Option<String>,
}

impl Item {
    pub(super) fn valid(&self) -> bool {
        (1..=64).contains(&self.sku.len())
            && self
                .sku
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
            && self.revision.as_ref().is_none_or(|value| {
                (1..=32).contains(&value.len())
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
    }
}

#[derive(Deserialize)]
pub(super) struct GrossPlan {
    sku: String,
    revision: String,
    pieces: String,
    tree: Vec<GrossNode>,
}

#[derive(Deserialize)]
struct GrossNode {
    sku: String,
    revision: Option<String>,
    pieces: String,
    children: Vec<GrossNode>,
}

#[derive(Debug, Serialize)]
pub(super) struct Node {
    pub item: Item,
    per_parent: i32,
    children: Vec<Node>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum NetError {
    InvalidStructure,
    QuantityLimit,
    UnconfiguredItem,
    InvalidBalance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct NetRow {
    pub requested: i32,
    pub reserved: i32,
}

impl NetRow {
    pub(super) fn uncovered(self) -> i32 {
        self.requested - self.reserved
    }
}

pub(super) fn positive(text: &str) -> Option<i32> {
    if !(1..=7).contains(&text.len())
        || text.starts_with('0')
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    text.parse()
        .ok()
        .filter(|value| (1..=MAX_PIECES).contains(value))
}

impl GrossPlan {
    pub(super) fn checked(self, root: &Item, pieces: i32) -> Result<Vec<Node>, NetError> {
        if !root.valid()
            || self.sku != root.sku
            || Some(&self.revision) != root.revision.as_ref()
            || positive(&self.pieces) != Some(pieces)
        {
            return Err(NetError::InvalidStructure);
        }
        let mut path = BTreeSet::from([root.clone()]);
        normalize(self.tree, pieces, &mut path, &mut 0)
    }
}

fn normalize(
    tree: Vec<GrossNode>,
    parent: i32,
    path: &mut BTreeSet<Item>,
    nodes: &mut usize,
) -> Result<Vec<Node>, NetError> {
    if tree.is_empty() || path.len() > MAX_DEPTH {
        return Err(NetError::InvalidStructure);
    }
    let mut result = Vec::new();
    for node in tree {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(NetError::QuantityLimit);
        }
        let item = Item {
            sku: node.sku,
            revision: node.revision,
        };
        let gross = positive(&node.pieces).ok_or(NetError::QuantityLimit)?;
        if !item.valid()
            || gross % parent != 0
            || (item.revision.is_none() && !node.children.is_empty())
        {
            return Err(NetError::InvalidStructure);
        }
        let children = if item.revision.is_some() {
            if !path.insert(item.clone()) {
                return Err(NetError::InvalidStructure);
            }
            let nested = normalize(node.children, gross, path, nodes)?;
            path.remove(&item);
            nested
        } else {
            vec![]
        };
        result.push(Node {
            item,
            per_parent: gross / parent,
            children,
        });
    }
    Ok(result)
}

pub(super) fn items(tree: &[Node]) -> BTreeSet<Item> {
    fn collect(tree: &[Node], result: &mut BTreeSet<Item>) {
        for node in tree {
            result.insert(node.item.clone());
            collect(&node.children, result);
        }
    }
    let mut result = BTreeSet::new();
    collect(tree, &mut result);
    result
}

pub(super) fn allocate(
    tree: &[Node],
    pieces: i32,
    mut available: BTreeMap<Item, i32>,
) -> Result<BTreeMap<Item, NetRow>, NetError> {
    fn walk(
        tree: &[Node],
        parent: i32,
        available: &mut BTreeMap<Item, i32>,
        rows: &mut BTreeMap<Item, NetRow>,
    ) -> Result<(), NetError> {
        for node in tree {
            let need = parent
                .checked_mul(node.per_parent)
                .filter(|value| *value <= MAX_PIECES)
                .ok_or(NetError::QuantityLimit)?;
            let stock = available
                .get_mut(&node.item)
                .ok_or(NetError::UnconfiguredItem)?;
            if *stock < 0 {
                return Err(NetError::InvalidBalance);
            }
            let held = need.min(*stock);
            *stock -= held;
            let row = rows.entry(node.item.clone()).or_default();
            row.requested = row
                .requested
                .checked_add(need)
                .filter(|value| *value <= MAX_PIECES)
                .ok_or(NetError::QuantityLimit)?;
            row.reserved += held; // Bounded by aggregated requested.
            let unmet = need - held;
            if unmet > 0 {
                walk(&node.children, unmet, available, rows)?;
            }
        }
        Ok(())
    }
    let mut rows = BTreeMap::new();
    walk(tree, pieces, &mut available, &mut rows)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    fn item(sku: &str, revision: Option<&str>) -> Item {
        Item {
            sku: sku.into(),
            revision: revision.map(str::to_string),
        }
    }

    #[test]
    fn stocked_subassembly_suppresses_its_children_and_shared_stock_is_spent_once() {
        let plan: GrossPlan = from_value(json!({
            "sku": "NM-1000", "revision": "A", "pieces": "10", "tree": [
                {"sku": "ASSEMBLY", "revision": "A", "pieces": "10", "children": [
                    {"sku": "SCREW", "revision": null, "pieces": "20", "children": []}
                ]},
                {"sku": "SCREW", "revision": null, "pieces": "10", "children": []}
            ]
        }))
        .unwrap();
        let tree = plan.checked(&item("NM-1000", Some("A")), 10).unwrap();
        let assembly = item("ASSEMBLY", Some("A"));
        let screw = item("SCREW", None);
        let rows = allocate(
            &tree,
            10,
            BTreeMap::from([(assembly.clone(), 6), (screw.clone(), 12)]),
        )
        .unwrap();
        assert_eq!(
            rows[&assembly],
            NetRow {
                requested: 10,
                reserved: 6
            }
        );
        assert_eq!(
            rows[&screw],
            NetRow {
                requested: 18,
                reserved: 12
            }
        );
        assert_eq!(rows[&screw].uncovered(), 6);
    }

    #[test]
    fn refuses_fractional_component_ratios_and_mismatched_pins() {
        let raw = json!({
            "sku": "NM-1000", "revision": "A", "pieces": "10", "tree": [
                {"sku": "SCREW", "revision": null, "pieces": "17", "children": []}
            ]
        });
        for revision in ["A", "B"] {
            let plan: GrossPlan = from_value(raw.clone()).unwrap();
            assert_eq!(
                plan.checked(&item("NM-1000", Some(revision)), 10)
                    .unwrap_err(),
                NetError::InvalidStructure
            );
        }
    }
}
