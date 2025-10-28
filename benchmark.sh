#!/bin/bash

echo "================================================"
echo "Performance Comparison: Go vs Rust"
echo "================================================"
echo ""

# Configuration
GO_URL="http://localhost:3000/api/trip-statistics"
RUST_URL="http://localhost:8080/api/v1/trip-statistics"
TOKEN="YOUR_JWT_TOKEN_HERE"
REQUESTS=1000
CONCURRENCY=50

echo "Testing Go Service..."
ab -n $REQUESTS -c $CONCURRENCY \
   -H "Authorization: Bearer $TOKEN" \
   "${GO_URL}?start_date=2024-01-01&end_date=2024-12-31" \
   > go_results.txt 2>&1

echo "Testing Rust Service (JSON)..."
ab -n $REQUESTS -c $CONCURRENCY \
   -H "Authorization: Bearer $TOKEN" \
   "${RUST_URL}?start_date=2024-01-01&end_date=2024-12-31" \
   > rust_json_results.txt 2>&1

echo "Testing Rust Service (MessagePack)..."
ab -n $REQUESTS -c $CONCURRENCY \
   -H "Authorization: Bearer $TOKEN" \
   "${RUST_URL}?start_date=2024-01-01&end_date=2024-12-31&format=msgpack" \
   > rust_msgpack_results.txt 2>&1

echo ""
echo "Results Summary:"
echo "================"

echo ""
echo "Go Service:"
grep "Requests per second" go_results.txt
grep "Time per request" go_results.txt | head -1

echo ""
echo "Rust Service (JSON):"
grep "Requests per second" rust_json_results.txt
grep "Time per request" rust_json_results.txt | head -1

echo ""
echo "Rust Service (MessagePack):"
grep "Requests per second" rust_msgpack_results.txt
grep "Time per request" rust_msgpack_results.txt | head -1

echo ""
echo "Full results saved to *_results.txt files"
