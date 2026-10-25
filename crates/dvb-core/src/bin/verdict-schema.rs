//! Generates the provider's strict verdict schema from the serde contract types.
fn main() -> Result<(), serde_json::Error> {
    print!("{}", dvb_core::run_verbs::verdict_schema_json()?);
    Ok(())
}
