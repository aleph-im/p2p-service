fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/aleph_p2p.proto")?;
    Ok(())
}
