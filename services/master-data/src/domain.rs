use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) use erp4us_identity::Scope;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterArticle {
    pub(crate) sku: String,
    pub(crate) name: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Article {
    #[serde(rename = "_id")]
    #[serde(with = "uuid::serde::hyphenated")]
    pub(crate) id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    pub(crate) tenant_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    pub(crate) organization_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    pub(crate) created_by: Uuid,
    pub(crate) schema_version: u32,
    pub(crate) revision: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub(crate) created_at: OffsetDateTime,
    pub(crate) sku: String,
    pub(crate) name: String,
}

impl Article {
    pub(crate) fn register(scope: Scope, command: RegisterArticle) -> Result<Self, &'static str> {
        let sku = command.sku;
        if !(1..=32).contains(&sku.len())
            || !sku
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("SKU must have 1–32 uppercase ASCII letters, digits or hyphens");
        }
        let name = command.name;
        if !(1..=120).contains(&name.chars().count())
            || name.chars().any(|c| c.is_ascii_control())
            || name.trim().is_empty()
        {
            return Err("Name must have 1–120 characters, no ASCII controls, and cannot be blank");
        }
        Ok(Self {
            id: Uuid::new_v4(),
            tenant_id: scope.tenant,
            organization_id: scope.organization,
            created_by: scope.actor,
            schema_version: 1,
            revision: 1,
            created_at: OffsetDateTime::now_utc(),
            sku,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(sku: &str, name: &str) -> Result<Article, &'static str> {
        Article::register(
            Scope {
                tenant: Uuid::new_v4(),
                organization: Uuid::new_v4(),
                actor: Uuid::new_v4(),
            },
            RegisterArticle {
                sku: sku.into(),
                name: name.into(),
            },
        )
    }

    #[test]
    fn rejects_invalid_skus_and_names() {
        for sku in ["", "mixed-Case", "ABC/DEF", &"A".repeat(33)] {
            assert!(register(sku, "Steel bracket").is_err());
        }
        for name in ["", " \t ", "Steel\nbracket", &"ä".repeat(121)] {
            assert!(register("BRACKET-1", name).is_err());
        }
    }

    #[test]
    fn accepts_unicode_names_at_the_character_limit() {
        assert!(register("BRACKET-1", &"ä".repeat(120)).is_ok());
    }
}
