//! Prints the generated contract; scripts/gen-contracts.sh is the entry point.

fn main() -> Result<(), serde_json::Error> {
    print!("{}", dvb_engine::api::openapi_json()?);
    Ok(())
}
