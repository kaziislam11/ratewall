//! Mock HRM backend stub.
//! Run: `mock-hrm` (defaults to port 3001).

use ratewall_mock_backends::common::serve;

#[tokio::main]
async fn main() {
    serve("hrm", 3001).await;
}
