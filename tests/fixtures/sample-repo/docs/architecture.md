# Sample Architecture

## Overview

The AuthService handles user authentication using JWT tokens.
It depends on Redis for session caching and PostgreSQL for user storage.

## AuthService

AuthService is the core authentication system.
It uses bcrypt for password hashing and supports OAuth2.
AuthService has been deprecated in favor of AuthV2.

## AuthV2

AuthV2 is the active replacement for AuthService.
AuthV2 requires TLS 1.3 for all connections.
