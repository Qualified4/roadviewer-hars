fn main() {
    let dir = "resources/schema/cereal";
    println!("cargo:rerun-if-changed={dir}");
    capnpc::CompilerCommand::new()
        .src_prefix(dir)
        .import_path(dir)
        .file(format!("{dir}/log.capnp"))
        .file(format!("{dir}/car.capnp"))
        .file(format!("{dir}/custom.capnp"))
        .file(format!("{dir}/deprecated.capnp"))
        .run()
        .expect("install capnproto to compile the preserved cereal schemas");
}
