//! Mock CRM backend stub.
//! Run: `mock-crm` (defaults to port 3000).

use ratewall_mock_backends::common::serve;

#[tokio::main]
async fn main() {
    serve("crm", 3000).await;
}
