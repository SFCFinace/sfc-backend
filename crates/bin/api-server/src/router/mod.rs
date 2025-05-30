use crate::{
    controller::{common_controller, swagger_controller},
    router::middware::route_logger,
};

use configs::{cfgs::Redis as RedisConfig, CFG};
use mongodb::Database; // Changed from sea_orm::DatabaseConnection
use redis::Client as RedisClient;
use salvo::Handler;
use salvo::cors::Cors;
use salvo::http::Method;
use salvo::{
    Router,
    Service,
    async_trait,
    catcher::Catcher,
    handler, // Import async_trait and handler macros
    logging::Logger,
    oapi::{OpenApi, Operation, swagger_ui::SwaggerUi},
    prelude::{CatchPanic, Depot, FlowCtrl, Request, Response, SessionHandler},
    serve_static::StaticDir,
    session::CookieStore,
};
use service::invoice::InvoiceService; // Import InvoiceService
use service::service::PurchaseService; // Import PurchaseService
use service::cache::InvoiceRedisService;
use service::repository::{TokenRepository, InvoiceRepository, EnterpriseRepository, UserRepository};
use service::service::TokenService;
use std::{env, sync::Arc};
use pharos_interact::{InvoiceContract, ContractQuerier, ContractWriter}; // Import for contract interaction
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Http, Provider};
use ethers::signers::LocalWallet;

pub mod middware;
pub mod router;
use router::{
    init_user_router, init_enterprise_router, init_invoice_router, 
    init_purchase_router, init_admin_router, init_transaction_router, 
    init_interest_router, init_token_router
}; 

// --- Injection Middleware Struct ---
#[derive(Clone)] // Clone is needed for the handler
struct InjectConnections {
    mongodb: Arc<Database>, // Changed from db_conn: Arc<DatabaseConnection>
    redis_client: Arc<RedisClient>,
    contract: Option<Arc<InvoiceContract<SignerMiddleware<Provider<Http>, LocalWallet>>>>, // Contract connection
    invoice_service: Arc<InvoiceService>, // Add InvoiceService
    purchase_service: Arc<PurchaseService>, // Add PurchaseService
    token_service: Arc<TokenService>, // Add TokenService
}

#[async_trait]
impl Handler for InjectConnections {
    async fn handle(&self, req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
        depot.inject(self.mongodb.clone()); // Updated
        depot.inject(self.redis_client.clone());
        depot.inject(self.invoice_service.clone()); // Inject InvoiceService
        depot.inject(self.purchase_service.clone()); // Inject PurchaseService
        depot.inject(self.token_service.clone()); // Inject TokenService
        
        // Inject contract connection if available
        if let Some(contract) = &self.contract {
            depot.inject(contract.clone());
        }
        
        // Indicate that the next handler should be called
        ctrl.call_next(req, depot, res).await;
    }
}

// init_router remains mostly the same, but doesn't add inject_connections middleware here
pub fn init_router() -> Router {
    let current_dir = env::current_dir().unwrap();
    log::warn!("Current working directory: {:?}", current_dir);
    let static_router = Router::with_path("/<**path>").get(StaticDir::new(current_dir.join("/static")).defaults("index.html").auto_list(true));

    // Base router without connection injection yet
    let router = Router::new().hoop(Logger::new()).hoop(CatchPanic::new()).push(static_router);

    // Business routes under /rwa prefix
    let api_router = Router::with_path(&CFG.server.api_prefix) // Use configured prefix
        .push(init_user_router()) // Existing user/auth routes
        .push(init_enterprise_router()) // Add enterprise routes
        .push(init_invoice_router()) // Keep non-RWA invoice routes if needed
        .push(init_purchase_router()) // Add RWA purchase routes
        .push(init_admin_router()) // Add admin routes
        .push(init_transaction_router()) // Add transaction routes 
        .push(init_interest_router()) // Add interest routes
        .push(init_token_router()); // Add token routes


    let router = router.push(api_router);

    // Swagger UI and docs setup
    let session_handler = SessionHandler::builder(CookieStore::new(), b"salvo-adminsalvo-adminalvo-adminsalvo-admin2023salvo-admin2023salvo-admin2023")
        .build()
        .unwrap();

    // OpenAPI Documentation
    let doc = OpenApi::new("Pharos-RWA", "0.1.1").merge_router(&router);

    let router = router.push(
        Router::new()
            .hoop(session_handler)
            .push(
                Router::new()
                    .hoop(swagger_controller::auth_token)
                    .push(doc.into_router("/api-doc/openapi.json"))
                    .push(SwaggerUi::new("/api-doc/openapi.json").into_router("swagger-ui")),
            )
            .push(Router::with_path("/swaggerLogin").post(swagger_controller::swagger_login)),
    );
    router
}

// Modify init_service to create and inject InvoiceService
pub fn init_service(
    mongodb: Arc<Database>, 
    redis_client: Arc<RedisClient>,
    contract: Option<Arc<InvoiceContract<SignerMiddleware<Provider<Http>, LocalWallet>>>>,
) -> Service {
    let router = init_router();

    // Create InvoiceService instance 
    let invoice_service = Arc::new(InvoiceService::new((*mongodb).clone(), (*redis_client).clone()));

    // Create Redis service for the PurchaseService
    let redis_service = Arc::new(InvoiceRedisService::new((*redis_client).clone()));
    
    // Create PurchaseService instance
    let purchase_service = Arc::new(PurchaseService::new(Arc::new(mongodb.client().clone()), redis_service));

    // Create repositories for the TokenService
    let token_repository = Arc::new(TokenRepository::new(mongodb.clone()));
    let invoice_repository = Arc::new(InvoiceRepository::new(&mongodb));
    let enterprise_repository = Arc::new(EnterpriseRepository::new(&mongodb));
    let user_repository = Arc::new(UserRepository::new(&mongodb));
    
    // Create TokenService instance
    let token_service = Arc::new(TokenService::new(
        token_repository,
        invoice_repository,
        enterprise_repository,
        user_repository,
        mongodb.clone()
    ));

    // Create the injector instance
    let injector = InjectConnections {
        mongodb, 
        redis_client,
        contract,
        invoice_service, // Inject the created service
        purchase_service, // Inject the PurchaseService
        token_service, // Inject the TokenService
    };
    let cors = Cors::new()
        .allow_origin("*")
        .allow_methods(vec![Method::GET, Method::POST,Method::PUT, Method::DELETE])
        .into_handler();
    // Apply CORS, then injection, then catcher, then router
    Service::new(router)
        .hoop(cors)
        .hoop(injector) // Use the injector instance
        .catcher(Catcher::default().hoop(common_controller::catcher_err))
}
