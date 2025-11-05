-- Create fleet_expenses table
CREATE TABLE IF NOT EXISTS fleet_expenses (
    id SERIAL PRIMARY KEY,
    car_no_plate VARCHAR(50),  -- Optional: NULL for general expenses
    expense_date DATE NOT NULL,
    expense_type VARCHAR(100) NOT NULL,
    amount NUMERIC(12, 2) NOT NULL CHECK (amount >= 0),
    description TEXT,
    company VARCHAR(100),  -- Optional: NULL for general expenses
    paid_by VARCHAR(255),  -- Who transferred/paid
    payment_method VARCHAR(50) NOT NULL CHECK (payment_method IN ('Cash', 'IPN Transfer')),
    created_by INTEGER NOT NULL,  -- User ID who created the record
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP  -- Soft delete support
);

-- Create indexes for better query performance
CREATE INDEX idx_fleet_expenses_car ON fleet_expenses(car_no_plate) WHERE deleted_at IS NULL;
CREATE INDEX idx_fleet_expenses_date ON fleet_expenses(expense_date) WHERE deleted_at IS NULL;
CREATE INDEX idx_fleet_expenses_company ON fleet_expenses(company) WHERE deleted_at IS NULL;
CREATE INDEX idx_fleet_expenses_type ON fleet_expenses(expense_type) WHERE deleted_at IS NULL;
CREATE INDEX idx_fleet_expenses_deleted ON fleet_expenses(deleted_at);
CREATE INDEX idx_fleet_expenses_created_by ON fleet_expenses(created_by);

-- Create updated_at trigger
CREATE OR REPLACE FUNCTION update_fleet_expenses_updated_at()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trigger_fleet_expenses_updated_at
    BEFORE UPDATE ON fleet_expenses
    FOR EACH ROW
    EXECUTE FUNCTION update_fleet_expenses_updated_at();

-- Add comments for documentation
COMMENT ON TABLE fleet_expenses IS 'Fleet expense tracking with soft delete support';
COMMENT ON COLUMN fleet_expenses.car_no_plate IS 'Optional: Vehicle plate number, NULL for general expenses';
COMMENT ON COLUMN fleet_expenses.company IS 'Optional: Company name (Petrol Arrows, TAQA, Petromin, Watanya), NULL for general expenses';
COMMENT ON COLUMN fleet_expenses.payment_method IS 'Payment method: Cash or IPN Transfer';
COMMENT ON COLUMN fleet_expenses.deleted_at IS 'Soft delete timestamp, NULL for active records';