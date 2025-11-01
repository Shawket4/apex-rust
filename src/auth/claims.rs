use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub user_type: String,
    pub user_id: Option<i32>,
    pub driver_id: Option<i32>,
    pub permission: Option<i32>,
    pub exp: i64,
}

impl Claims {
    pub fn is_admin(&self) -> bool {
        self.permission >= 3
    }
    
    pub fn is_driver(&self) -> bool {
        self.user_type == "driver"
    }
    
    pub fn has_permission(&self, required: i32, user_permission: i32) -> bool {
        self.is_admin() && user_permission >= required
    }
}
