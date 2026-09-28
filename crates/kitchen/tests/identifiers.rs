//! Public identifier contract tests.

use kitchen::{HouseId, IdentifierError, TaskId};

#[test]
fn identities_preserve_case_and_round_trip() -> Result<(), IdentifierError> {
    let house = HouseId::new("House_2-west")?;
    let task: TaskId = "Task-42".parse()?;
    assert_eq!(house.as_str(), "House_2-west");
    assert_eq!(task.to_string(), "Task-42");
    assert_ne!(HouseId::new("House")?, HouseId::new("house")?);
    Ok(())
}

#[test]
fn leading_digits_and_trailing_or_repeated_separators_are_accepted() -> Result<(), IdentifierError> {
    for value in ["42", "9-task", "a-", "a_", "a--b"] {
        assert_eq!(HouseId::new(value)?.as_str(), value);
        assert_eq!(TaskId::new(value)?.as_str(), value);
    }
    Ok(())
}

#[test]
fn length_boundaries_are_inclusive() -> Result<(), IdentifierError> {
    for value in ["a".to_owned(), "a".repeat(64)] {
        assert_eq!(HouseId::new(&value)?.as_str(), value);
        assert_eq!(TaskId::new(&value)?.as_str(), value);
    }
    for value in [String::new(), "a".repeat(65)] {
        let expected = IdentifierError::Length {
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
        assert_eq!(HouseId::new(value), Err(IdentifierError::Characters));
        assert_eq!(TaskId::new(value), Err(IdentifierError::Characters));
    }
}

#[test]
fn rejected_input_is_not_exposed_in_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    let error = HouseId::new("private/secret")
        .err()
        .ok_or("invalid input was accepted")?;
    assert_eq!(error, IdentifierError::Characters);
    assert!(!error.to_string().contains("private/secret"));
    Ok(())
}
