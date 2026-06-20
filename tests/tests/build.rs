use std::{fs, path::Path, process::Command};

use insta::assert_snapshot;
use tempfile::tempdir;

use crate::rojo_test::io_util::{get_working_dir_path, BUILD_TESTS_PATH, ROJO_PATH};

macro_rules! gen_build_tests {
    ( $($test_name: ident,)* ) => {
        $(
            paste::item! {
                #[test]
                fn [<build_ $test_name>]() {
                    let _ = env_logger::try_init();

                    run_build_test(stringify!($test_name));
                }
            }
        )*
    };
}

gen_build_tests! {
    init_csv_with_children,
    attributes,
    client_in_folder,
    client_init,
    csv_bug_145,
    csv_bug_147,
    csv_in_folder,
    deep_nesting,
    gitkeep,
    ignore_glob_inner,
    ignore_glob_nested,
    ignore_glob_spec,
    infer_service_name,
    infer_starter_player,
    init_meta_class_name,
    init_meta_properties,
    init_with_children,
    issue_546,
    json_as_lua,
    json_model_in_folder,
    json_model_legacy_name,
    module_in_folder,
    module_init,
    nested_runcontext,
    optional,
    project_composed_default,
    project_composed_file,
    project_root_name,
    rbxm_in_folder,
    rbxmx_in_folder,
    rbxmx_ref,
    script_meta_disabled,
    server_in_folder,
    server_init,
    txt,
    txt_in_folder,
    unresolved_values,
    weldconstraint,
    sync_rule_alone,
    sync_rule_complex,
    sync_rule_nested_projects,
    no_name_default_project,
    no_name_project,
    no_name_top_level_project,
    plugin_init,
}

/// A `postBuild` hook should run after a successful build, and `--no-hooks`
/// should skip it. Uses a shell hook that writes a marker file (portable across
/// the `sh`/`cmd` used on the CI matrix) and checks for the marker's presence.
#[test]
fn build_runs_post_build_hook_unless_disabled() {
    let _ = env_logger::try_init();

    let dir = tempdir().expect("couldn't create temporary directory");
    let project_dir = dir.path();

    fs::write(
        project_dir.join("default.project.json"),
        r#"{
            "name": "hooktest",
            "hooks": { "postBuild": ["echo built > hook_marker.txt"] },
            "tree": { "$className": "Folder" }
        }"#,
    )
    .expect("couldn't write project file");

    let output_path = project_dir.join("out.rbxmx");
    let marker = project_dir.join("hook_marker.txt");

    let build = |extra_args: &[&str]| {
        let mut args = vec![
            "build",
            project_dir.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ];
        args.extend_from_slice(extra_args);
        Command::new(ROJO_PATH)
            .args(&args)
            .env("RUST_LOG", "error")
            .status()
            .expect("couldn't start Rojo")
    };

    // Hooks enabled: the postBuild hook runs and creates the marker.
    assert!(build(&[]).success(), "rojo build failed");
    assert!(
        marker.exists(),
        "the postBuild hook should have created the marker file"
    );

    fs::remove_file(&marker).expect("couldn't remove marker");

    // --no-hooks: the hook is skipped, so the marker is not recreated.
    assert!(
        build(&["--no-hooks"]).success(),
        "rojo build --no-hooks failed"
    );
    assert!(
        !marker.exists(),
        "--no-hooks should have skipped the postBuild hook"
    );
}

fn run_build_test(test_name: &str) {
    let working_dir = get_working_dir_path();

    let input_path = Path::new(BUILD_TESTS_PATH).join(test_name);

    let output_dir = tempdir().expect("couldn't create temporary directory");
    let output_path = output_dir.path().join(format!("{}.rbxmx", test_name));

    let output = Command::new(ROJO_PATH)
        .args([
            "build",
            input_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .env("RUST_LOG", "error")
        .current_dir(working_dir)
        .output()
        .expect("Couldn't start Rojo");

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    assert!(output.status.success(), "Rojo did not exit successfully");

    let contents = fs::read_to_string(&output_path).expect("Couldn't read output file");

    let mut settings = insta::Settings::new();

    let snapshot_path = Path::new(BUILD_TESTS_PATH)
        .parent()
        .unwrap()
        .join("build-test-snapshots");

    settings.set_snapshot_path(snapshot_path);

    settings.bind(|| {
        assert_snapshot!(test_name, contents);
    });
}
