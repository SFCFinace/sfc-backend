use ethers::types::{Address, Signature};
use moka::future::Cache;
use rand::RngCore;
use salvo::oapi::{ToSchema, extract::JsonBody};
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::utils::res::{Res, ResObj, res_json_custom, res_json_err, res_json_ok};
use chrono::Utc;
use log::{error, info, warn};
use salvo::http::header;
use serde_json::json;

use service::repository::UserRepository;
use mongodb::Database;
use thiserror::Error;
use crate::controller::Claims;
use jsonwebtoken::{encode, Header, EncodingKey, Algorithm};
use configs::CFG;
use service::repository::EnterpriseRepository;
use common::domain::entity::Enterprise;
use mongodb::bson::oid::ObjectId;

// --- Nonce Cache ---
lazy_static::lazy_static! {
    // Cache stores nonce (String) keyed by a unique request ID (String)
    static ref NONCE_CACHE: Cache<String, String> = Cache::builder()
        // Time to live: Nonces expire after 5 minutes
        .time_to_live(Duration::from_secs(5 * 60))
        // Time to idle: Nonces expire if not accessed for 5 minutes
        .time_to_idle(Duration::from_secs(5 * 60))
        // Maximum capacity of the cache
        .max_capacity(10_000)
        .build();
}

// --- Error Handling ---
#[derive(Debug, Error, Serialize, ToSchema)]
pub enum AuthError {
    #[error("Nonce not found or expired")]
    NonceNotFound,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Invalid address format")]
    InvalidAddress,
    #[error("Internal server error: {0}")]
    Internal(String),
}

// --- API Structures ---
#[derive(Deserialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "address": "0x..."})))]
pub struct ChallengeRequest {
    #[serde(rename = "address")]
    pub address: String, // Wallet address requesting the challenge
}

#[derive(Serialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "nonce": "...", "requestId": "..."})))]
pub struct ChallengeResponse {
    pub nonce: String,
    #[serde(rename = "requestId")]
    pub request_id: String, // Unique ID to link challenge and login
}

#[derive(Deserialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "requestId": "...", "signature": "0x..."})))]
pub struct LoginRequest {
    #[serde(rename = "requestId")]
    pub request_id: String, // ID received from /challenge
    pub signature: String, // Signature generated by the wallet
}

#[derive(Serialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "token": "eyJ...", "walletAddress": "0x..."})))]
pub struct LoginResponse {
    pub token: String, // The generated JWT
    #[serde(rename = "walletAddress")]
    pub wallet_address: String, // Return wallet address as confirmation
}

#[derive(Deserialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "enterpriseAddress": "0x..."})))]
pub struct BindEnterpriseRequest {
    #[serde(rename = "enterpriseAddress")]
    pub enterprise_address: String,
}

// 用户绑定的企业信息响应
#[derive(Serialize, ToSchema, Debug)]
#[salvo(schema(example = json!({ "isEnterpriseBound": true, "enterpriseName": "Acme Corp", "enterpriseAddress": "0x..." })))]
pub struct EnterpriseInfoResponse {
    #[serde(rename = "isEnterpriseBound")]
    pub is_enterprise_bound: bool,
    #[serde(rename = "enterpriseName", skip_serializing_if = "Option::is_none")]
    pub enterprise_name: Option<String>,
    #[serde(rename = "enterpriseAddress", skip_serializing_if = "Option::is_none")]
    pub enterprise_address: Option<String>,
    #[serde(rename = "enterpriseId", skip_serializing_if = "Option::is_none")]
    pub enterprise_id: Option<String>,
}

// --- Handlers ---

/// 登录步骤1 生成一个挑战
#[salvo::oapi::endpoint(
    tags("用户"),
    status_codes(200, 400),
    request_body = ChallengeRequest,
    responses(
        (status_code = 200, description = "Challenge generated successfully."),
        (status_code = 400, description = "Invalid request."),
    )
)]
pub async fn challenge(req: JsonBody<ChallengeRequest>, depot: &mut Depot) -> Res<ChallengeResponse> {
    let address_str = &req.address;

    // Basic validation (more thorough validation might be needed)
    if !address_str.starts_with("0x") || address_str.len() != 42 {
        warn!("Invalid address format received: {}", address_str);
        return Err(res_json_custom(400, "InvalidAddress"));
    }

    let nonce = generate_nonce();
    let request_id = Uuid::new_v4().to_string();

    // Store nonce associated with the request ID
    NONCE_CACHE.insert(request_id.clone(), nonce.clone()).await;
    info!("Generated nonce for request ID: {}", request_id);

    Ok(res_json_ok(Some(ChallengeResponse { nonce, request_id })))
}

/// 登录步骤2 验证挑战并登录 (generates JWT)
#[salvo::oapi::endpoint(
    tags("用户"),
    status_codes(200, 400, 401, 500),
    request_body = LoginRequest,
    responses(
        (status_code = 200, description = "Login successful, JWT returned.", body = LoginResponse),
        (status_code = 400, description = "Nonce not found or expired / Invalid signature format."),
        (status_code = 401, description = "Invalid signature (verification failed)."),
        (status_code = 500, description = "Internal server error during login processing."),
    )
)]
pub async fn login(req: JsonBody<LoginRequest>, depot: &mut Depot, request: &mut Request) -> Res<LoginResponse> {
    // Retrieve MongoDB database from Depot
    let mongodb = depot.obtain::<Arc<Database>>().expect("MongoDB Database connection not found in Depot").clone();
    
    // Create user repository
    let user_repo = UserRepository::new(&mongodb);

    // 1. Retrieve the nonce from the cache
    let request_id = &req.request_id;
    let signature_str = &req.signature;

    let nonce = match NONCE_CACHE.get(request_id).await {
        Some(n) => {
            // Invalidate the nonce after retrieval to prevent reuse
            NONCE_CACHE.invalidate(request_id).await;
            n
        }
        None => {
            warn!("Nonce not found or expired for request ID: {}", request_id);
            return Err(res_json_custom(400, "NonceNotFoundOrExpired"));
        }
    };

    // 2. Prepare the message that was signed (should match exactly what frontend signed)
    let message_to_verify = nonce; 

    // 3. Parse the signature
    let signature: Signature = match signature_str.parse() {
        Ok(sig) => sig,
        Err(e) => {
            warn!("Invalid signature format provided: {}", e);
            // Return 400 for bad format
            return Err(res_json_custom(400, "InvalidSignatureFormat"));
        }
    };

    // 4. Recover the address that signed the message
    let recovered_address = match signature.recover(message_to_verify) {
        Ok(addr) => addr,
        Err(e) => {
            warn!("Failed to recover address from signature: {}", e);
            // Return 401 as signature verification failed
            return Err(res_json_custom(401, "InvalidSignature"));
        }
    };

    // Format recovered address consistently (lowercase hex)
    let recovered_address_str = format!("0x{:x}", recovered_address).to_lowercase();
    info!("Successfully recovered address: {}", recovered_address_str);

    // 5. Process user login (find or create user based on recovered address)
    let user = match user_repo.process_login(&recovered_address_str).await {
        Ok(db_user) => {
            info!("Processed login for user: {}", recovered_address_str);
            db_user // Keep the user object if needed later, otherwise ignore
        }
        Err(e) => {
            error!("Database error processing user login for {}: {}", recovered_address_str, e);
            // Return 500 for internal errors
            return Err(res_json_custom(500, "LoginProcessingError"));
        }
    };

    // 6. Generate JWT
    let now = Utc::now();
    // Set expiration (e.g., 1 day from now)
    let expiration_time = now + chrono::Duration::days(1);
    let exp_timestamp = expiration_time.timestamp() as usize;

    // Convert user role to string
    let role_str = match user.role {
        common::domain::entity::UserRole::Investor => "investor",
        common::domain::entity::UserRole::EnterpriseAdmin => "creditor",
        common::domain::entity::UserRole::PlatformAdmin => "admin",
    };

    let claims = Claims {
        sub: recovered_address_str.clone(), // Use recovered address as subject
        exp: exp_timestamp,
        user_id: user.id.unwrap().to_hex(), // Add user_id field
        role: role_str.to_string(), // Add role field
    };

    // Retrieve the secret key from configuration
    let secret = &CFG.jwt.secret;
    let encoding_key = EncodingKey::from_secret(secret.as_ref());
    
    let token = match encode(&Header::new(Algorithm::HS256), &claims, &encoding_key) {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to generate JWT: {}", e);
            return Err(res_json_custom(500, "TokenGenerationError"));
        }
    };

    // 7. Return successful response with JWT and wallet address
    Ok(res_json_ok(Some(LoginResponse {
        token,
        wallet_address: recovered_address_str,
    })))    
}

/// 绑定用户到企业 (Requires authentication)
#[salvo::oapi::endpoint(
    tags("用户"),
    status_codes(200, 400, 401, 404, 500),
    request_body = BindEnterpriseRequest,
    responses(
        (status_code = 200, description = "Successfully bound user to enterprise."),
        (status_code = 400, description = "Invalid enterprise address format."),
        (status_code = 401, description = "User not authenticated."),
        (status_code = 404, description = "Enterprise not found with the provided address."),
        (status_code = 500, description = "Internal server error."),
    )
)]
pub async fn bind_enterprise(req: JsonBody<BindEnterpriseRequest>, depot: &mut Depot) -> Res<()> { // Returns Res<()> for success/failure
    // 1. Get authenticated user address from depot (inserted by auth_token middleware)
    let user_address = match depot.get::<String>("user_address") {
        Ok(address_ref) => {
            address_ref.as_str()
        },
        Err(e) => {
            log::error!("Authenticated user address not found or wrong type in depot: {:?}", e);
            return Err(res_json_err("User not authenticated"));
        }
    };

    // 2. Get dependencies
    let mongodb = depot.obtain::<Arc<Database>>().expect("Database connection not found").clone();
    let user_repo = UserRepository::new(&mongodb);
    let enterprise_repo = EnterpriseRepository::new(&mongodb);

    // 3. Validate enterprise address format
    let enterprise_address = &req.enterprise_address;
    if !enterprise_address.starts_with("0x") || enterprise_address.len() != 42 {
        warn!("Invalid enterprise address format provided for binding: {}", enterprise_address);
        return Err(res_json_err( "InvalidEnterpriseAddressFormat"));
    }

    // 4. Find the enterprise by its wallet address
    let enterprise_oid = match enterprise_repo.find_by_wallet_address(enterprise_address).await {
        Ok(Some(enterprise)) => {
            if let Some(id) = enterprise.id {
                id // Return the ObjectId
            } else {
                 error!("Enterprise found by address {} but has no ObjectId", enterprise_address);
                 return Err(res_json_custom(500, "EnterpriseMissingId"));
            }
        },
        Ok(None) => {
            log::warn!("Enterprise not found with address: {}", enterprise_address);
            return Err(res_json_custom(404, "EnterpriseNotFound"));
        }
        Err(e) => {
            error!("Database error finding enterprise by address {}: {}", enterprise_address, e);
            return Err(res_json_custom(500, "DatabaseError"));
        }
    };

    // 5. Bind the user to the enterprise
    match user_repo.bind_enterprise(&user_address, enterprise_oid).await {
        Ok(true) => {
            info!("Successfully bound user {} to enterprise {}", user_address, enterprise_oid);
            Ok(res_json_ok(None)) // Return 200 OK with no body
        }
        Ok(false) => {
             // This means the user address wasn't found, which shouldn't happen if they are authenticated
             error!("Authenticated user {} not found in DB for binding update?", user_address);
             Err(res_json_custom(500, "AuthenticatedUserNotFoundInDB"))
        }
        Err(e) => {
            error!("Database error binding user {} to enterprise {}: {}", user_address, enterprise_oid, e);
            Err(res_json_custom(500, "DatabaseError"))
        }
    }
}

/// 获取用户绑定的企业信息 (需要认证)
#[salvo::oapi::endpoint(
    tags("用户"),
    status_codes(200, 401, 500),
    responses(
        (status_code = 200, description = "获取用户绑定的企业信息", body = EnterpriseInfoResponse),
        (status_code = 401, description = "用户未认证"),
        (status_code = 500, description = "内部服务器错误"),
    )
)]
pub async fn get_enterprise_info(depot: &mut Depot) -> Res<EnterpriseInfoResponse> {
    // 1. 获取已认证用户的地址（由auth_token中间件插入）
    let user_address = match depot.get::<String>("user_address") {
        Ok(address_ref) => {
            address_ref.as_str()
        },
        Err(e) => {
            log::error!("Authenticated user address not found or wrong type in depot: {:?}", e);
            return Err(res_json_err("User not authenticated"));
        }
    };

    // 2. 获取依赖
    let mongodb = depot.obtain::<Arc<Database>>().expect("Database connection not found").clone();
    let user_repo = UserRepository::new(&mongodb);
    let enterprise_repo = EnterpriseRepository::new(&mongodb);

    // 3. 查找用户
    let user = match user_repo.find_by_wallet_address(user_address).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            error!("Authenticated user not found in database: {}", user_address);
            return Err(res_json_custom(500, "AuthenticatedUserNotFound"));
        },
        Err(e) => {
            error!("Database error finding user by address {}: {}", user_address, e);
            return Err(res_json_custom(500, "DatabaseError"));
        }
    };

    // 4. 检查用户是否绑定了企业
    if let Some(enterprise_id) = user.enterprise_id {
        // 5. 如果绑定了企业，获取企业信息
        match enterprise_repo.find_by_id(enterprise_id).await {
            Ok(Some(enterprise)) => {
                let response = EnterpriseInfoResponse {
                    is_enterprise_bound: true,
                    enterprise_name: Some(enterprise.name),
                    enterprise_address: Some(enterprise.wallet_address),
                    enterprise_id: Some(enterprise_id.to_string()),
                };
                Ok(res_json_ok(Some(response)))
            },
            Ok(None) => {
                // 找到了用户绑定的企业ID，但企业不存在
                warn!("Enterprise with ID {} bound to user {} not found", enterprise_id, user_address);
                let response = EnterpriseInfoResponse {
                    is_enterprise_bound: true,
                    enterprise_name: None,
                    enterprise_address: None,
                    enterprise_id: Some(enterprise_id.to_string()),
                };
                Ok(res_json_ok(Some(response)))
            },
            Err(e) => {
                error!("Database error finding enterprise by ID {}: {}", enterprise_id, e);
                return Err(res_json_custom(500, "DatabaseError"));
            }
        }
    } else {
        // 6. 如果未绑定企业，返回未绑定状态
        let response = EnterpriseInfoResponse {
            is_enterprise_bound: false,
            enterprise_name: None,
            enterprise_address: None,
            enterprise_id: None,
        };
        Ok(res_json_ok(Some(response)))
    }
}

// --- Helper Functions ---
fn generate_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("pharos-auth-{}", hex::encode(bytes))
}
