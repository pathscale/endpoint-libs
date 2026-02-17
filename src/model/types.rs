use serde::*;

/// `Field` is a struct that represents the parameters and returns in an endpoint schema.
#[derive(Clone, Debug, Hash, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct Field {
    /// The name of the field (e.g. `user_id`)
    pub name: String,

    /// The description of the field
    #[serde(skip)]
    pub description: String,

    /// The type of the field (e.g. `Type::BigInt`)
    pub ty: Type,
}

impl Field {
    /// Creates a new `Field` with the given name and type.
    /// `description` is set to `""`.
    pub fn new(name: impl Into<String>, ty: Type) -> Self {
        Self {
            name: name.into(),
            description: "".into(),
            ty,
        }
    }

    /// Creates a new `Field` with the given name, type and description.
    pub fn new_with_description(
        name: impl Into<String>,
        description: impl Into<String>,
        ty: Type,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            ty,
        }
    }
}

/// `EnumVariant` is a struct that represents the variants of an enum.
#[derive(Clone, Debug, Hash, Serialize, Deserialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct EnumVariant {
    /// The name of the variant (e.g. `UniSwap`)
    pub name: String,

    /// A description added by `new_with_description` method
    pub description: String,

    /// The value of the variant (e.g. 1)
    pub value: i64,
}

impl EnumVariant {
    /// Creates a new `EnumVariant` with the given name and value.
    /// `description` is set to `""`.
    pub fn new(name: impl Into<String>, value: i64) -> Self {
        Self {
            name: name.into(),
            description: "".into(),
            value,
        }
    }

    /// Creates a new `EnumVariant` with the given name, value and description.
    pub fn new_with_description(
        name: impl Into<String>,
        description: impl Into<String>,
        value: i64,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            value,
        }
    }
}

/// `Type` is an enum that represents the types of the fields in an endpoint schema.
#[derive(Clone, Debug, Serialize, Deserialize, Hash, PartialEq, PartialOrd, Eq, Ord)]
pub enum Type {
    Date,
    Int,
    BigInt,
    Numeric,
    Boolean,
    String,
    Bytea,
    UUID,
    Inet,
    Struct {
        name: String,
        fields: Vec<Field>,
    },
    StructRef(String),
    Object,
    DataTable {
        name: String,
        fields: Vec<Field>,
    },
    StructTable {
        struct_ref: String,
    },
    Vec(Box<Type>),
    Unit,
    Optional(Box<Type>),
    Enum {
        name: String,
        variants: Vec<EnumVariant>,
    },
    EnumRef {
        name: String,
        #[serde(default, skip_serializing)]
        prefixed_name: bool,
    },
    TimeStampMs,
    BlockchainDecimal,
    BlockchainAddress,
    BlockchainTransactionHash,
}

impl Type {
    /// Creates a new `Type::Struct` with the given name and fields.
    pub fn struct_(name: impl Into<String>, fields: Vec<Field>) -> Self {
        Self::Struct {
            name: name.into(),
            fields,
        }
    }

    /// Creates a new `Type::StructRef` with the given name.
    pub fn struct_ref(name: impl Into<String>) -> Self {
        Self::StructRef(name.into())
    }

    /// Creates a new `Type::DataTable` with the given name and fields.
    pub fn datatable(name: impl Into<String>, fields: Vec<Field>) -> Self {
        Self::DataTable {
            name: name.into(),
            fields,
        }
    }

    /// Creates a new `Type::StructTable` with the given struct reference.
    pub fn struct_table(struct_ref: impl Into<String>) -> Self {
        Self::StructTable {
            struct_ref: struct_ref.into(),
        }
    }

    /// Creates a new `Type::Vec` with the given type.
    pub fn vec(ty: Type) -> Self {
        Self::Vec(Box::new(ty))
    }

    /// Creates a new `Type::Optional` with the given type.
    pub fn optional(ty: Type) -> Self {
        Self::Optional(Box::new(ty))
    }

    /// Creates a new `Type::EnumRef` with the given name.
    pub fn enum_ref(name: impl Into<String>, prefixed_name: bool) -> Self {
        Self::EnumRef {
            name: name.into(),
            prefixed_name,
        }
    }

    /// Creates a new `Type::Enum` with the given name and fields/variants.
    pub fn enum_(name: impl Into<String>, fields: Vec<EnumVariant>) -> Self {
        Self::Enum {
            name: name.into(),
            variants: fields,
        }
    }
    pub fn try_unwrap(self) -> Option<Self> {
        match self {
            Self::Vec(v) => Some(*v),
            Self::DataTable { .. } => None,
            Self::StructTable { .. } => None,
            _ => Some(self),
        }
    }

    pub fn add_default_enum_derives(input: String) -> String {
        format!(
            r#"#[derive(Debug, Clone, Copy, Serialize, Deserialize, FromPrimitive, PartialEq, Eq, PartialOrd, Ord, EnumString, Display, Hash)]{input}"#
        )
    }

    pub fn add_default_struct_derives(input: String) -> String {
        format!(
            r#" #[derive(Serialize, Deserialize, Debug, Clone)]
                #[serde(rename_all = "camelCase")]
                {input}
            "#
        )
    }
}
