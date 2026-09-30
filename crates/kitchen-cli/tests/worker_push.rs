//! The worker push command has no externally callable credential output.

use std::process::Command;

#[test]
fn no_cli_invocation_exposes_a_forge_token() -> Result<(), Box<dyn std::error::Error>> {
    let token = "fixture-forge-token-never-print";
    let cases: &[&[&str]] = &[
        &["push", "--help"],
        &[
            "push",
            "--store",
            "/does/not/exist",
            "--house",
            "house",
            "--task",
            "task",
            "--credential-helper",
            "get",
        ],
        &[
            "push",
            "--store",
            "/does/not/exist",
            "--house",
            "house",
            "--task",
            "task",
        ],
    ];
    for args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_kitchn"))
            .args(*args)
            .env("GH_TOKEN", token)
            .output()?;
        let output_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output_text.contains(token), "token appeared for {args:?}");
        if args.contains(&"--credential-helper") {
            assert!(!output.status.success(), "removed helper was accepted");
        }
    }
    Ok(())
}
