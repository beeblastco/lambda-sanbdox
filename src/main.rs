use lambda_agent_sandbox::handler;
use lambda_runtime::{run, service_fn, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}
