package main

import (
    "bytes"
    "encoding/json"
    "fmt"
    "io"
    "net/http"
    "time"
    
    "github.com/vmihailenco/msgpack/v5"
)

// RustMicroserviceClient handles communication with the Rust microservice
type RustMicroserviceClient struct {
    BaseURL    string
    HTTPClient *http.Client
}

// TripStatisticsResponse matches the Rust response structure
type TripStatisticsResponse struct {
    Message            string              `json:"message" msgpack:"message"`
    Data               []TripStatistics    `json:"data" msgpack:"data"`
    StatsByDate        []interface{}       `json:"stats_by_date" msgpack:"stats_by_date"`
    HasFinancialAccess bool                `json:"has_financial_access" msgpack:"has_financial_access"`
    CarTotals          []CarTotal          `json:"car_totals" msgpack:"car_totals"`
}

type TripStatistics struct {
    Company      string                  `json:"company" msgpack:"company"`
    TotalTrips   int64                   `json:"total_trips" msgpack:"total_trips"`
    TotalVolume  float64                 `json:"total_volume" msgpack:"total_volume"`
    TotalDistance float64                `json:"total_distance" msgpack:"total_distance"`
    TotalRevenue float64                 `json:"total_revenue" msgpack:"total_revenue"`
    Details      []TripStatisticsDetails `json:"details" msgpack:"details"`
    RouteDetails []interface{}           `json:"route_details" msgpack:"route_details"`
}

type TripStatisticsDetails struct {
    GroupName     string   `json:"group_name" msgpack:"group_name"`
    TotalTrips    int64    `json:"total_trips" msgpack:"total_trips"`
    TotalVolume   float64  `json:"total_volume" msgpack:"total_volume"`
    TotalDistance float64  `json:"total_distance" msgpack:"total_distance"`
    TotalRevenue  *float64 `json:"total_revenue,omitempty" msgpack:"total_revenue,omitempty"`
}

type CarTotal struct {
    CarNoPlate  string  `json:"car_no_plate" msgpack:"car_no_plate"`
    Liters      float64 `json:"liters" msgpack:"liters"`
    Distance    float64 `json:"distance" msgpack:"distance"`
    BaseRevenue float64 `json:"base_revenue" msgpack:"base_revenue"`
    VAT         float64 `json:"vat" msgpack:"vat"`
    Rent        float64 `json:"rent" msgpack:"rent"`
}

// NewRustClient creates a new client for the Rust microservice
func NewRustClient(baseURL string) *RustMicroserviceClient {
    return &RustMicroserviceClient{
        BaseURL: baseURL,
        HTTPClient: &http.Client{
            Timeout: 30 * time.Second,
        },
    }
}

// GetTripStatisticsJSON fetches trip statistics in JSON format
func (c *RustMicroserviceClient) GetTripStatisticsJSON(token, startDate, endDate, company string) (*TripStatisticsResponse, error) {
    url := fmt.Sprintf("%s/api/v1/trip-statistics?start_date=%s&end_date=%s", 
        c.BaseURL, startDate, endDate)
    
    if company != "" {
        url += "&company=" + company
    }
    
    req, err := http.NewRequest("GET", url, nil)
    if err != nil {
        return nil, err
    }
    
    req.Header.Set("Authorization", "Bearer "+token)
    req.Header.Set("Accept", "application/json")
    
    resp, err := c.HTTPClient.Do(req)
    if err != nil {
        return nil, err
    }
    defer resp.Body.Close()
    
    if resp.StatusCode != http.StatusOK {
        body, _ := io.ReadAll(resp.Body)
        return nil, fmt.Errorf("unexpected status code: %d, body: %s", resp.StatusCode, string(body))
    }
    
    var result TripStatisticsResponse
    if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
        return nil, err
    }
    
    return &result, nil
}

// GetTripStatisticsMsgPack fetches trip statistics in MessagePack format (faster)
func (c *RustMicroserviceClient) GetTripStatisticsMsgPack(token, startDate, endDate, company string) (*TripStatisticsResponse, error) {
    url := fmt.Sprintf("%s/api/v1/trip-statistics?start_date=%s&end_date=%s&format=msgpack", 
        c.BaseURL, startDate, endDate)
    
    if company != "" {
        url += "&company=" + company
    }
    
    req, err := http.NewRequest("GET", url, nil)
    if err != nil {
        return nil, err
    }
    
    req.Header.Set("Authorization", "Bearer "+token)
    req.Header.Set("Accept", "application/msgpack")
    
    resp, err := c.HTTPClient.Do(req)
    if err != nil {
        return nil, err
    }
    defer resp.Body.Close()
    
    if resp.StatusCode != http.StatusOK {
        body, _ := io.ReadAll(resp.Body)
        return nil, fmt.Errorf("unexpected status code: %d, body: %s", resp.StatusCode, string(body))
    }
    
    body, err := io.ReadAll(resp.Body)
    if err != nil {
        return nil, err
    }
    
    var result TripStatisticsResponse
    if err := msgpack.Unmarshal(body, &result); err != nil {
        return nil, err
    }
    
    return &result, nil
}

// Example usage in your existing Fiber handler
func ExampleFiberHandler(c *fiber.Ctx) error {
    client := NewRustClient("http://localhost:8080")
    
    // Get JWT token from cookie or header
    token := c.Cookies("jwt")
    if token == "" {
        authHeader := c.Get("Authorization")
        if len(authHeader) > 7 && authHeader[:7] == "Bearer " {
            token = authHeader[7:]
        }
    }
    
    startDate := c.Query("start_date")
    endDate := c.Query("end_date")
    company := c.Query("company")
    
    // Use MessagePack for better performance
    stats, err := client.GetTripStatisticsMsgPack(token, startDate, endDate, company)
    if err != nil {
        return c.Status(fiber.StatusInternalServerError).JSON(fiber.Map{
            "message": "Failed to fetch statistics from Rust service",
            "error":   err.Error(),
        })
    }
    
    return c.Status(fiber.StatusOK).JSON(stats)
}

func main() {
    // Example usage
    client := NewRustClient("http://localhost:8080")
    
    stats, err := client.GetTripStatisticsMsgPack(
        "your-jwt-token",
        "2024-01-01",
        "2024-12-31",
        "Watanya",
    )
    
    if err != nil {
        fmt.Printf("Error: %v\n", err)
        return
    }
    
    fmt.Printf("Retrieved statistics for %d companies\n", len(stats.Data))
    for _, company := range stats.Data {
        fmt.Printf("Company: %s, Trips: %d, Revenue: %.2f\n", 
            company.Company, company.TotalTrips, company.TotalRevenue)
    }
}
