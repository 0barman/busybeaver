use super::Beaver;
use crate::ShutdownOptions;
use std::error::Error;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.into()))
}

#[tokio::test]
async fn repeated_shutdown_reuses_the_winning_process_without_new_allocations() -> TestResult {
    let beaver = Beaver::new("shutdown-process-reuse", 4)?;
    let options = ShutdownOptions::new();
    let first = beaver.shutdown(options.clone())?;
    let second = beaver.shutdown(options)?;
    if beaver.shutdown_process_creations_for_test() != 1 {
        return Err(test_error(
            "repeated shutdown created more than one private shutdown process",
        ));
    }
    if first.id() != second.id() {
        return Err(test_error(
            "repeated shutdown did not return the winning shutdown handle",
        ));
    }
    let _ = first.wait_final().await?;
    Ok(())
}
