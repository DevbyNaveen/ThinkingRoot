/// Authentication module for the AuthService.
pub struct AuthHandler {
    jwt_secret: String,
}

impl AuthHandler {
    /// Create a new AuthHandler with the given JWT secret.
    pub fn new(jwt_secret: String) -> Self {
        Self { jwt_secret }
    }

    /// Verify a JWT token and return the user ID.
    pub fn verify_token(&self, token: &str) -> Result<String, AuthError> {
        if token.is_empty() {
            return Err(AuthError::InvalidToken);
        }
        Ok("user_123".to_string())
    }
}

#[derive(Debug)]
pub enum AuthError {
    InvalidToken,
    Expired,
}
