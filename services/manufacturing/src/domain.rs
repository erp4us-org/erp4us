use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub(crate) products: Vec<Product>,
    pub(crate) recipes: Vec<Recipe>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Product {
    pub(crate) sku: String,
    pub(crate) name: String,
    pub(crate) revision: String,
    pub(crate) price_eur: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    sku: String,
    revision: String,
    components: Vec<Component>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Component {
    sku: String,
    name: String,
    pieces: u32,
    revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Demand {
    sku: String,
    revision: String,
    pieces: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlanError {
    InvalidQuantity,
    UnknownProduct,
    InvalidStructure,
    Cycle,
    Overflow,
}

#[derive(Serialize)]
pub(crate) struct Plan {
    sku: String,
    revision: String,
    pieces: String,
    tree: Vec<Node>,
    pub(crate) materials: Vec<Material>,
}

#[derive(Serialize)]
struct Node {
    sku: String,
    name: String,
    pieces: String,
    revision: Option<String>,
    children: Vec<Node>,
}

#[derive(Serialize)]
pub(crate) struct Material {
    sku: String,
    name: String,
    pieces: String,
    unit: &'static str,
}

impl Catalog {
    pub(crate) fn validate(&self) -> Result<(), PlanError> {
        if self.products.is_empty() {
            return Err(PlanError::InvalidStructure);
        }
        let mut keys = BTreeSet::new();
        for product in &self.products {
            if !keys.insert((&product.sku, &product.revision)) {
                return Err(PlanError::InvalidStructure);
            }
            self.plan(Demand {
                sku: product.sku.clone(),
                revision: product.revision.clone(),
                pieces: "1".into(),
            })?;
        }
        Ok(())
    }

    pub(crate) fn plan(&self, demand: Demand) -> Result<Plan, PlanError> {
        if demand.pieces.starts_with('0') || !demand.pieces.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PlanError::InvalidQuantity);
        }
        let quantity: u32 = demand
            .pieces
            .parse()
            .map_err(|_| PlanError::InvalidQuantity)?;
        if !(1..=1000).contains(&quantity) {
            return Err(PlanError::InvalidQuantity);
        }
        if !self
            .products
            .iter()
            .any(|product| product.sku == demand.sku && product.revision == demand.revision)
        {
            return Err(PlanError::UnknownProduct);
        }
        let mut recipes = BTreeMap::new();
        for recipe in &self.recipes {
            if recipes
                .insert((recipe.sku.as_str(), recipe.revision.as_str()), recipe)
                .is_some()
            {
                return Err(PlanError::InvalidStructure);
            }
        }
        let mut explosion = Explosion {
            recipes,
            path: BTreeSet::new(),
            leaves: BTreeMap::new(),
            edges: 0,
        };
        let tree = explosion.expand((&demand.sku, &demand.revision), quantity)?;
        let materials = explosion
            .leaves
            .into_iter()
            .map(|(sku, (name, pieces))| Material {
                sku: sku.to_owned(),
                name: name.to_owned(),
                pieces: pieces.to_string(),
                unit: "EA",
            })
            .collect();
        Ok(Plan {
            sku: demand.sku,
            revision: demand.revision,
            pieces: demand.pieces,
            tree,
            materials,
        })
    }
}

struct Explosion<'a> {
    recipes: BTreeMap<(&'a str, &'a str), &'a Recipe>,
    path: BTreeSet<(&'a str, &'a str)>,
    leaves: BTreeMap<&'a str, (&'a str, u32)>,
    edges: usize,
}

impl<'a> Explosion<'a> {
    fn expand(&mut self, key: (&'a str, &'a str), quantity: u32) -> Result<Vec<Node>, PlanError> {
        if self.path.len() >= 16 {
            return Err(PlanError::InvalidStructure);
        }
        if !self.path.insert(key) {
            return Err(PlanError::Cycle);
        }
        let recipe = self
            .recipes
            .get(&key)
            .copied()
            .ok_or(PlanError::InvalidStructure)?;
        if recipe.components.is_empty() {
            return Err(PlanError::InvalidStructure);
        }
        let mut children = Vec::new();
        for child in &recipe.components {
            self.edges += 1;
            if self.edges > 1024 || child.pieces == 0 {
                return Err(PlanError::InvalidStructure);
            }
            let total = quantity
                .checked_mul(child.pieces)
                .ok_or(PlanError::Overflow)?;
            let nested = if let Some(revision) = &child.revision {
                self.expand((&child.sku, revision), total)?
            } else {
                let (name, sum) = self.leaves.entry(&child.sku).or_insert((&child.name, 0));
                if *name != child.name {
                    return Err(PlanError::InvalidStructure);
                }
                *sum = sum.checked_add(total).ok_or(PlanError::Overflow)?;
                Vec::new()
            };
            children.push(Node {
                sku: child.sku.clone(),
                name: child.name.clone(),
                pieces: total.to_string(),
                revision: child.revision.clone(),
                children: nested,
            });
        }
        self.path.remove(&key);
        Ok(children)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Catalog {
        serde_json::from_str(include_str!("novamotion.json")).unwrap()
    }
    fn demand(pieces: &str) -> Demand {
        Demand {
            sku: "NM-1000".into(),
            revision: "A".into(),
            pieces: pieces.into(),
        }
    }

    #[test]
    fn explodes_both_subassemblies_for_ten_modules() {
        let plan = fixture().plan(demand("10")).unwrap();
        let needs: BTreeMap<_, _> = plan
            .materials
            .iter()
            .map(|m| (m.sku.as_str(), m.pieces.as_str()))
            .collect();
        assert_eq!(
            needs,
            BTreeMap::from([
                ("NM-PROFILE", "20"),
                ("NM-ENDPLATE", "20"),
                ("NM-SCREW", "120"),
                ("NM-MOTOR", "10"),
                ("NM-COUPLING", "10"),
                ("NM-SPINDLE", "10"),
                ("NM-RAIL", "20"),
                ("NM-SENSOR", "20"),
                ("NM-CABLE", "10"),
                ("NM-PACK", "10"),
            ])
        );
        assert_eq!(plan.tree[0].sku, "NM-1100");
        assert_eq!(plan.tree[1].sku, "NM-1200");
    }

    #[test]
    fn rejects_cycles_unknown_revision_and_unbounded_arithmetic() {
        let mut catalog = fixture();
        catalog.recipes[1].components[0].sku = "NM-1000".into();
        catalog.recipes[1].components[0].revision = Some("A".into());
        assert_eq!(catalog.plan(demand("10")).err(), Some(PlanError::Cycle));
        let mut catalog = fixture();
        catalog.recipes[1].components[0].pieces = u32::MAX;
        assert_eq!(catalog.plan(demand("10")).err(), Some(PlanError::Overflow));
        let mut missing = demand("10");
        missing.revision = "B".into();
        assert_eq!(
            fixture().plan(missing).err(),
            Some(PlanError::UnknownProduct)
        );
        for invalid in ["0", "1.0", "-1", "1001", " 10", "010"] {
            assert_eq!(
                fixture().plan(demand(invalid)).err(),
                Some(PlanError::InvalidQuantity)
            );
        }
    }
}
