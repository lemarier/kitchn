//! Public identifier contract tests.

use kitchen::{Error, HouseId, TaskId, id::validate_identifier};

#[test]
fn identities_preserve_case_and_round_trip() -> Result<(), Error> {
    let house = HouseId::new("House_2-west")?;
    let task: TaskId = "Task-42".parse()?;
    assert_eq!(house.as_str(), "House_2-west");
    assert_eq!(task.to_string(), "Task-42");
    assert_ne!(HouseId::new("House")?, HouseId::new("house")?);
    Ok(())
}

#[test]
fn length_boundaries_are_inclusive() -> Result<(), Error> {
    for value in ["a".to_owned(), "a".repeat(64)] {
        assert_eq!(HouseId::new(&value)?.as_str(), value);
        assert_eq!(TaskId::new(&value)?.as_str(), value);
    }
    for value in [String::new(), "a".repeat(65)] {
        let expected = Error::IdentifierLength {
            actual: value.len(),
        };
        assert_eq!(HouseId::new(&value), Err(expected.clone()));
        assert_eq!(TaskId::new(&value), Err(expected));
    }
    Ok(())
}

#[test]
fn unsafe_or_ambiguous_text_is_rejected_without_normalization() {
    for value in [
        "-a", "_a", " a", "a ", "a/b", "a\\b", "..", "a.b", "é", "a\n", "a\0",
    ] {
        assert_eq!(validate_identifier(value), Err(Error::IdentifierCharacters));
        assert_eq!(HouseId::new(value), Err(Error::IdentifierCharacters));
        assert_eq!(TaskId::new(value), Err(Error::IdentifierCharacters));
    }
}

#[test]
fn rejected_input_is_not_exposed_in_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    let error = HouseId::new("private/secret")
        .err()
        .ok_or("invalid input was accepted")?;
    assert_eq!(error, Error::IdentifierCharacters);
    assert!(!error.to_string().contains("private/secret"));
    Ok(())
}
