use endpoint_libs::model::{EndpointSchema, EnumVariant, Field, Type, TypeRegistry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = EndpointSchema::new(
        "Run",
        1,
        Vec::new(),
        vec![
            Field::new(
                "status",
                Type::enum_("Status", vec![EnumVariant::new("Accepted", 1)]),
            ),
            Field::new("output", Type::optional(Type::String)),
        ],
    );
    let schema = endpoint.to_mcp_output_schema(&TypeRegistry::new())?;
    println!("{schema}");
    Ok(())
}
