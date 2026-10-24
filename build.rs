// `sqlx::migrate!` embeds the migrations directory at compile time, and cargo
// does not otherwise notice a new file there: without this, a build after
// adding a migration silently ships the old set.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
