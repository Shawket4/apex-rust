# Trip Statistics Rust Microservice

A high-performance, production-ready microservice for trip statistics written in Rust.

## Features

- 🚀 **High Performance**: Optimized SQL queries with minimal application-level processing
- 🔒 **JWT Authentication**: Secure authentication with role-based access control
- 📦 **MessagePack Support**: Efficient binary serialization for better performance
- 🗄️ **PostgreSQL**: Robust database with connection pooling
- 🔄 **CORS Enabled**: Ready for frontend integration
- 📊 **Comprehensive Statistics**: Multiple company-specific calculation methods

## Prerequisites

- Rust 1.70+ (installed via rustup)
- PostgreSQL 12+
- Cargo

## Quick Start

### 1. Configure Database

Edit `.env` file with your database credentials:

```env
DATABASE_URL=postgresql://username:password@localhost:5432/your_database
JWT_SECRET=your-secret-key-change-this
RUST_LOG=info
SERVER_HOST=127.0.0.1
SERVER_PORT=8080
WORKERS=4
```

### 2. Install Dependencies

```bash
cargo build
```

### 3. Run the Service

**Development mode with auto-reload:**
```bash
cargo install cargo-watch
cargo watch -x run
```

**Production mode:**
```bash
cargo build --release
./target/release/apex
```

## API Endpoints

### Health Check
```bash
GET /health
```

### Trip Statistics
```bash
GET /api/v1/trip-statistics?start_date=2024-01-01&end_date=2024-12-31&company=Watanya&format=msgpack

Headers:
  Authorization: Bearer <jwt_token>
  # OR
  Cookie: jwt=<jwt_token>
```

**Query Parameters:**
- `start_date` (required): Start date (YYYY-MM-DD)
- `end_date` (required): End date (YYYY-MM-DD)
- `company` (optional): Filter by specific company
- `format` (optional): Response format (`json` or `msgpack`)

## Response Formats

### JSON (default)
```bash
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:8080/api/v1/trip-statistics?start_date=2024-01-01&end_date=2024-12-31"
```

### MessagePack (faster)
```bash
curl -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:8080/api/v1/trip-statistics?start_date=2024-01-01&end_date=2024-12-31&format=msgpack" \
  --output response.msgpack
```

## Performance Optimizations

1. **Database-Heavy Processing**: Complex aggregations handled by PostgreSQL
2. **Connection Pooling**: Efficient database connection management
3. **Zero-Copy Serialization**: MessagePack for minimal overhead
4. **Compile-Time Optimizations**: LTO and aggressive optimization flags

## Project Structure

```
apex/
├── src/
│   ├── main.rs              # Application entry point
│   ├── config.rs            # Configuration management
│   ├── auth/                # Authentication module
│   │   ├── mod.rs
│   │   ├── claims.rs        # JWT claims
│   │   └── middleware.rs    # Auth middleware
│   ├── models/              # Data models
│   │   ├── mod.rs
│   │   ├── trip.rs
│   │   └── response.rs
│   ├── handlers/            # HTTP handlers
│   │   ├── mod.rs
│   │   └── trip_stats.rs
│   ├── db/                  # Database layer
│   │   ├── mod.rs
│   │   └── queries.rs
│   └── utils/               # Utilities
│       ├── mod.rs
│       └── msgpack.rs
├── Cargo.toml
├── .env
└── README.md
```

## Integration with Go Service

### Option 1: Reverse Proxy (Recommended)

Add to your Go service's router:

```go
// Proxy performance-critical endpoints to Rust
http.HandleFunc("/api/v1/trip-statistics", func(w http.ResponseWriter, r *http.Request) {
    url := "http://localhost:8080" + r.URL.Path + "?" + r.URL.RawQuery
    proxyReq, _ := http.NewRequest(r.Method, url, r.Body)
    
    // Copy headers
    for key, values := range r.Header {
        for _, value := range values {
            proxyReq.Header.Add(key, value)
        }
    }
    
    client := &http.Client{}
    resp, err := client.Do(proxyReq)
    if err != nil {
        http.Error(w, err.Error(), http.StatusInternalServerError)
        return
    }
    defer resp.Body.Close()
    
    // Copy response
    for key, values := range resp.Header {
        for _, value := range values {
            w.Header().Add(key, value)
        }
    }
    w.WriteHeader(resp.StatusCode)
    io.Copy(w, resp.Body)
})
```

### Option 2: Direct Client

```go
package client

import (
    "bytes"
    "encoding/json"
    "fmt"
    "io"
    "net/http"
)

type RustClient struct {
    BaseURL string
    Client  *http.Client
}

func NewRustClient(baseURL string) *RustClient {
    return &RustClient{
        BaseURL: baseURL,
        Client:  &http.Client{Timeout: 30 * time.Second},
    }
}

func (c *RustClient) GetTripStatistics(token, startDate, endDate string) ([]byte, error) {
    url := fmt.Sprintf("%s/api/v1/trip-statistics?start_date=%s&end_date=%s&format=msgpack",
        c.BaseURL, startDate, endDate)
    
    req, _ := http.NewRequest("GET", url, nil)
    req.Header.Set("Authorization", "Bearer "+token)
    
    resp, err := c.Client.Do(req)
    if err != nil {
        return nil, err
    }
    defer resp.Body.Close()
    
    return io.ReadAll(resp.Body)
}
```

## Benchmarking

```bash
# Install Apache Bench
sudo apt-get install apache2-utils

# Benchmark
ab -n 1000 -c 10 -H "Authorization: Bearer YOUR_TOKEN" \
  "http://localhost:8080/api/v1/trip-statistics?start_date=2024-01-01&end_date=2024-12-31"
```

## Development

### Run Tests
```bash
cargo test
```

### Check Code
```bash
cargo clippy -- -D warnings
cargo fmt --check
```

### Watch Mode
```bash
cargo watch -x "run --release"
```

## Deployment

### Docker

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libpq5 ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/apex /usr/local/bin/
CMD ["apex"]
```

Build and run:
```bash
docker build -t apex .
docker run -p 8080:8080 --env-file .env apex
```

### Systemd Service

```ini
[Unit]
Description=Trip Statistics Rust Service
After=network.target postgresql.service

[Service]
Type=simple
User=www-data
WorkingDirectory=/opt/apex
EnvironmentFile=/opt/apex/.env
ExecStart=/opt/apex/apex
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

## Monitoring

Add to `Cargo.toml`:
```toml
actix-web-prom = "0.7"
```

Then in main.rs:
```rust
use actix_web_prom::PrometheusMetricsBuilder;

let prometheus = PrometheusMetricsBuilder::new("api")
    .endpoint("/metrics")
    .build()
    .unwrap();

App::new()
    .wrap(prometheus.clone())
    // ... rest of config
```

## Troubleshooting

### Database Connection Issues
```bash
# Test connection
psql -h localhost -U username -d your_database

# Check environment variables
echo $DATABASE_URL
```

### Port Already in Use
```bash
# Find process using port 8080
lsof -i :8080

# Kill process
kill -9 <PID>
```

### Performance Issues
- Enable release mode: `cargo build --release`
- Increase worker count in `.env`
- Optimize PostgreSQL with proper indexes
- Use MessagePack format for responses

## License

MIT

## Support

For issues and questions, please create an issue in the repository.
