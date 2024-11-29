//! `SeaORM` Entity, @generated by sea-orm-codegen 1.1.0

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "folders")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub org: String,
    pub folder_id: String,
    pub name: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub description: Option<String>,
    pub r#type: i16,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::dashboards::Entity")]
    Dashboards,
}

impl Related<super::dashboards::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dashboards.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
