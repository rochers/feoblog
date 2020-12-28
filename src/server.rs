use std::{borrow::Cow, fmt};

use futures_core::stream::Stream;
use futures_util::StreamExt;

use actix_web::http::header;
use actix_web::web::{
    self,
    get,
    put,
    resource,
    route,
    Data,
    Form,
    HttpResponse,
    Path,
    HttpRequest,
    Payload,
};
use actix_web::{App, HttpServer, Responder};
use askama::Template;
use failure::{bail, ResultExt, format_err};
use rust_embed::RustEmbed;

use actix_web::http::StatusCode;
use async_trait::async_trait;

use protobuf::Message;

use crate::{ServeCommand, backend::ItemProfileRow, protos::Item_oneof_item_type};
use crate::backend::{self, Backend, Factory, UserID, Signature, ItemRow, Timestamp};
use crate::protos::{Item, Post, ProtoValid};

mod filters;

pub(crate) fn serve(command: ServeCommand) -> Result<(), failure::Error> {

    env_logger::init();

    let ServeCommand{open, shared_options: options, mut binds} = command;

    // TODO: Error if the file doesn't exist, and make a separate 'init' command.
    let factory = backend::sqlite::Factory::new(options.sqlite_file.clone());
    // For now, this creates one if it doesn't exist already:
    factory.open()?.setup().context("Error setting up DB")?;
    

    let app_factory = move || {
        let mut app = App::new()
            .wrap(actix_web::middleware::Logger::default())
            .data(AppData{
                backend_factory: Box::new(factory.clone()),
            })
            .configure(routes)
        ;

        app = app.default_service(route().to(|| file_not_found("")));

        return app;
    };

    if binds.is_empty() {
        binds.push("127.0.0.1:8080".into());
    }

    let mut server = HttpServer::new(app_factory); 
    for bind in &binds {
        server = server.bind(bind)?;
    }

    if open {
        // TODO: This opens up a (AFAICT) blocking CLI browser on Linux. Boo. Don't do that.
        // TODO: Handle wildcard addresses (0.0.0.0, ::0) and open them via localhost.
        let url = format!("http://{}/", binds[0]);
        let opened = webbrowser::open(&url);
        if !opened.is_ok() {
            println!("Warning: Couldn't open browser.");
        }
    }

    for bind in &binds {
        println!("Started at: http://{}/", bind);
    }
 
    let mut system = actix_web::rt::System::new("web server");
    system.block_on(server.run())?;
   
    Ok(())
}

/// Data available for our whole application.
/// Gets stored in a Data<AppData>
// This is so that we have typesafe access to AppData fields, because actix
// Data<Foo> can fail at runtime if you delete a Foo and don't clean up after
// yourself.
struct AppData {
    backend_factory: Box<dyn backend::Factory>,
}

fn routes(cfg: &mut web::ServiceConfig) {
    cfg
        .route("/", get().to(index))

        .route("/u/{user_id}/", get().to(get_user_items))

        .route("/u/{userID}/i/{signature}/", get().to(show_item))
        .route("/u/{userID}/i/{signature}/proto3", put().to(put_item))
        .route("/u/{userID}/i/{signature}/proto3", get().to(get_item))


        .route("/u/{user_id}/profile/", get().to(show_profile))

    ;
    statics(cfg);
}

#[async_trait]
trait StaticFilesResponder {
    type Response: Responder;
    async fn response(path: Path<(String,)>) -> Result<Self::Response, Error>;
}

#[async_trait]
impl <T: RustEmbed> StaticFilesResponder for T {
    type Response = HttpResponse;

    async fn response(path: Path<(String,)>) -> Result<Self::Response, Error> {
        let (mut path,) = path.into_inner();
        
            
        let mut maybe_bytes = T::get(path.as_str());
        
        // Check index.html:
        if maybe_bytes.is_none() && (path.ends_with("/") || path.is_empty()) {
            let inner = format!("{}index.html", path);
            let mb = T::get(inner.as_str());
            if mb.is_some() {
                path = inner;
                maybe_bytes = mb;
            }
        }

        if let Some(bytes) = maybe_bytes {
            // Set some response headers.
            // In particular, a mime type is required for things like JS to work.
            let mime_type = format!("{}", mime_guess::from_path(path).first_or_octet_stream());
            let response = HttpResponse::Ok()
                .content_type(mime_type)

                // TODO: This likely will result in lots of byte copying.
                // Should implement our own MessageBody
                // for Cow<'static, [u8]>
                .body(bytes.into_owned());
            return Ok(response)
        }

        // If adding the slash would get us an index.html, do so:
        let with_index = format!("{}/index.html", path);
        if T::get(with_index.as_str()).is_some() {
            // Use a relative redirect from the inner-most path part:
            let part = path.split("/").last().expect("at least one element");
            let part = format!("{}/", part);
            return Ok(
                HttpResponse::SeeOther()
                    .header("location", part)
                    .finish()
            );
        }

        Ok(
            HttpResponse::NotFound()
            .body("File not found.")
        )
    }
} 


#[derive(RustEmbed, Debug)]
#[folder = "static/"]
struct StaticFiles;

#[derive(RustEmbed, Debug)]
#[folder = "web-client/build/"]
struct WebClientBuild;


fn statics(cfg: &mut web::ServiceConfig) {
    cfg
        .route("/static/{path:.*}", get().to(StaticFiles::response))
        .route("/client/{path:.*}", get().to(WebClientBuild::response))
    ;
}

/// The root (`/`) page.
async fn index(data: Data<AppData>) -> Result<impl Responder, Error> {
    let max_items = 10;
    let mut items = Vec::with_capacity(max_items);

    let mut item_callback = |row: ItemProfileRow| {        
        let mut item = Item::new();
        item.merge_from_bytes(&row.item.item_bytes)?;

        if display_by_default(&item) {
            items.push(IndexPageItem{row, item});
        }
        
        Ok(items.len() < max_items)
    };

    let max_time = Timestamp::now();
    let backend = data.backend_factory.open().compat()?;
    backend.homepage_items(max_time, &mut item_callback).compat()?;

    let response = IndexPage {
        nav: vec![
            Nav::Text("FeoBlog".into()),
            Nav::Link{
                text: "Client".into(),
                href: "/client/".into(),
            }
        ],
        posts: items,
    };

    Ok(response)
}

/// Display a single user's posts/etc.
/// `/u/{userID}/`
async fn get_user_items(
    data: Data<AppData>,
    path: Path<(UserID,)>
) -> Result<impl Responder, Error> {
    let max_items = 10;
    let mut items = Vec::with_capacity(max_items);

    let mut collect_items = |row: ItemRow| -> Result<bool, failure::Error>{
        let mut item = Item::new();
        item.merge_from_bytes(&row.item_bytes)?;

        // TODO: Option: show_all=1.
        if display_by_default(&item) {
            items.push(UserPageItem{ row, item });
        }

        Ok(items.len() < max_items)
    };

    // TODO: Support pagination.
    let max_time = Timestamp::now();

    let (user,) = path.into_inner();
    let backend = data.backend_factory.open().compat()?;
    backend.user_items(&user, max_time, &mut collect_items).compat()?;

    
    let mut nav = vec![];
    let profile = backend.user_profile(&user).compat()?;
    if let Some(row) = profile {
        let mut item = Item::new();
        item.merge_from_bytes(&row.item_bytes)?;

        nav.push(
            Nav::Text(item.get_profile().display_name.clone())
        )
    }

    nav.extend(vec![
        Nav::Link{
            text: "Profile".into(),
            href: format!("/u/{}/profile/", user.to_base58()),
        },
        Nav::Link{
            text: "Home".into(),
            href: "/".into()
        },
    ]);

    let page = UserPage{
        nav,
        posts: items,
    };

    Ok(page)
}

const MAX_ITEM_SIZE: usize = 1024 * 32; 
const PLAINTEXT: &'static str = "text/plain; charset=utf-8";

/// Accepts a proto3 Item
/// Returns 201 if the PUT was successful.
/// Returns 202 if the item already exists.
/// Returns ??? if the user lacks permission to post.
/// Returns ??? if the signature is not valid.
/// Returns a text body message w/ OK/Error message.
async fn put_item(
    data: Data<AppData>,
    path: Path<(String, String,)>,
    req: HttpRequest,
    mut body: Payload,
) -> Result<impl Responder, Error> 
{
    let (user_path, sig_path) = path.into_inner();
    let user = UserID::from_base58(user_path.as_str()).context("decoding user ID").compat()?;
    let signature = Signature::from_base58(sig_path.as_str()).context("decoding signature").compat()?;

    let length = match req.headers().get("content-length") {
        Some(length) => length,
        None => {
            return Ok(
                HttpResponse::BadRequest()
                .content_type(PLAINTEXT)
                .body("Must include length header.".to_string())
                // ... so that we can reject things that are too large outright.
            );
        }
    };

    let length: usize = match length.to_str()?.parse() {
        Ok(length) => length,
        Err(_) => {
            return Ok(
                HttpResponse::BadRequest()
                .content_type(PLAINTEXT)
                .body("Must include length header.".to_string())
            );
        },
    };

    if length > MAX_ITEM_SIZE {
        return Ok(
            HttpResponse::PayloadTooLarge()
            .content_type(PLAINTEXT)
            .body("Item too large".to_string())
        );
    }

    let mut backend = data.backend_factory.open().compat()?;
    // TODO: Eventually also check if this user is "followed". Their content
    // can be posted here too.
    let can_post = backend.server_user(&user).context("Loading server user").compat()?.is_some();

    if !can_post {
        return Ok(
            HttpResponse::Forbidden()
            .content_type(PLAINTEXT)
            .body("Not accepting Items for this user".to_string())
        )
    }

    // If the content already exists, do nothing: 202 Accepted?
    if backend.user_item_exists(&user, &signature).compat()? {
        return Ok(
            HttpResponse::Accepted()
            .content_type(PLAINTEXT)
            .body("Item already exists")
        );
    }
    
    let mut bytes: Vec<u8> = Vec::with_capacity(length);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("Error parsing chunk").compat()?;
        bytes.extend_from_slice(&chunk);
    }

    if !signature.is_valid(&user, &bytes) {
        Err(format_err!("Invalid signature").compat())?;
    }

    let mut item: Item = Item::new();
    item.merge_from_bytes(&bytes)?;
    item.validate()?;

    let message = format!("OK. Received {} bytes.", bytes.len());
    
    let row = ItemRow{
        user: user,
        signature: signature,
        timestamp: Timestamp{ unix_utc_ms: item.get_timestamp_ms_utc()},
        received: Timestamp::now(),
        item_bytes: bytes,
    };

    backend.save_user_item(&row, &item).context("Error saving user item").compat()?;

    let response = HttpResponse::Created()
        .content_type(PLAINTEXT)
        .body(message);

    Ok(response)
}


async fn show_item(
    data: Data<AppData>,
    path: Path<(UserID, Signature,)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> {

    let (user_id, signature) = path.into_inner();
    let backend = data.backend_factory.open().compat()?;
    let row = backend.user_item(&user_id, &signature).compat()?;
    let row = match row {
        Some(row) => row,
        None => { 
            // TODO: We could display a nicer error page here, showing where
            // the user might find this item on other servers. Maybe I'll leave that
            // for the in-browser client.

            return Ok(
                file_not_found("No such item").await
                .respond_to(&req).await?
            );
        }
    };

    let mut item = Item::new();
    item.merge_from_bytes(row.item_bytes.as_slice())?;

    let row = backend.user_profile(&user_id).compat()?;
    let display_name = {
        let mut item = Item::new();
        if let Some(row) = row {
            item.merge_from_bytes(row.item_bytes.as_slice())?;
        }
        item
    }.get_profile().display_name.clone();
    
    use crate::protos::Item_oneof_item_type as ItemType;
    match item.item_type {
        None => Ok(HttpResponse::InternalServerError().body("No known item type provided.")),
        Some(ItemType::profile(p)) => Ok(HttpResponse::Ok().body("Profile update.")),
        Some(ItemType::post(p)) => {
            let page = PostPage {
                nav: vec![
                    Nav::Text(display_name.clone()),
                    Nav::Link {
                        text: "Profile".into(),
                        href: format!("/u/{}/profile/", user_id.to_base58()),
                    },
                    Nav::Link {
                        text: "Home".into(),
                        href: "/".into()
                    }
                ],
                user_id,
                display_name,
                signature,
                text: p.body,
                title: p.title,
                timestamp_utc_ms: item.timestamp_ms_utc,
                utc_offset_minutes: item.utc_offset_minutes,
            };

            Ok(page.respond_to(&req).await?)
        },
    }


}

/// Get the binary representation of the item.
///
/// `/u/{userID}/i/{sig}/proto3`
async fn get_item(
    data: Data<AppData>,
    path: Path<(UserID, Signature,)>,
) -> Result<HttpResponse, Error> {
    
    let (user_id, signature) = path.into_inner();
    let backend = data.backend_factory.open().compat()?;
    let item = backend.user_item(&user_id, &signature).compat()?;
    let item = match item {
        Some(item) => item,
        None => { 
            return Ok(
                HttpResponse::NotFound().body("No such item")
            );
        }
    };

    // We could in theory validate the bytes ourselves, but if a client is directly fetching the 
    // protobuf bytes via this endpoint, it's probably going to be so that it can verify the bytes
    // for itself anyway.
    Ok(
        HttpResponse::Ok()
        .content_type("application/protobuf3")
        .body(item.item_bytes)
    )

}

async fn file_not_found(msg: impl Into<String>) -> impl Responder<Error=actix_web::error::Error> {
    NotFoundPage {
        message: msg.into()
    }
        .with_status(StatusCode::NOT_FOUND)
}

/// `/u/{userID}/profile/`
async fn show_profile(
    data: Data<AppData>,
    path: Path<(UserID,)>,
    req: HttpRequest,
) -> Result<HttpResponse, Error> 
{
    let (user_id,) = path.into_inner();
    let backend = data.backend_factory.open().compat()?;

    let row = backend.user_profile(&user_id).compat()?;

    let row = match row {
        Some(r) => r,
        None => {
            return Ok(HttpResponse::NotFound().body("No such user, or profile."))
        }
    };

    let mut item = Item::new();
    item.merge_from_bytes(&row.item_bytes)?;
    let display_name = item.get_profile().display_name.clone();
    let nav = vec![
        Nav::Text(display_name.clone()),
        // TODO: Add an Edit link. Make abstract w/ a link provider trait.
        Nav::Link{
            text: "Home".into(),
            href: "/".into(),
        },
    ];

    let timestamp_utc_ms = item.timestamp_ms_utc;
    let utc_offset_minutes = item.utc_offset_minutes;
    let text = std::mem::take(&mut item.mut_profile().about);

    let follows = std::mem::take(&mut item.get_profile()).follows.to_vec();
    let follows = follows.into_iter().map(|mut follow: crate::protos::Follow | -> Result<ProfileFollow, Error>{
        let mut user = std::mem::take(follow.mut_user());
        let user_id = UserID::from_vec(std::mem::take(&mut user.bytes)).compat()?;
        let display_name = follow.display_name;
        Ok(
            ProfileFollow{user_id, display_name}
        )
    }).collect::<Result<_,_>>()?;

    let page = ProfilePage{
        nav,
        text,
        display_name,
        follows,
        timestamp_utc_ms,
        utc_offset_minutes,
        user_id: row.user,
        signature: row.signature,
    };

    Ok(page.respond_to(&req).await?)
}


#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundPage {
    message: String,
}

#[derive(Template)]
#[template(path = "index.html")] 
struct IndexPage {
    nav: Vec<Nav>,
    posts: Vec<IndexPageItem>,
}

#[derive(Template)]
#[template(path = "user_page.html")]
struct UserPage {
    nav: Vec<Nav>,
    posts: Vec<UserPageItem>,
}

#[derive(Template)]
#[template(path = "profile.html")]
struct ProfilePage {
    nav: Vec<Nav>,
    user_id: UserID,
    signature: Signature,
    display_name: String,
    text: String,
    follows: Vec<ProfileFollow>,
    timestamp_utc_ms: i64,
    utc_offset_minutes: i32,
}

#[derive(Template)]
#[template(path = "post.html")]
struct PostPage {
    nav: Vec<Nav>,
    user_id: UserID,
    signature: Signature,
    display_name: String,
    text: String,
    title: String,
    timestamp_utc_ms: i64,
    utc_offset_minutes: i32,

    // TODO: Include comments from people this user follows.
}

struct ProfileFollow {
    /// May be ""
    display_name: String,
    user_id: UserID,
}

/// An Item we want to display on a page.
struct IndexPageItem {
    row: ItemProfileRow,
    item: Item,
}

impl IndexPageItem {
    fn item(&self) -> &Item { &self.item }
    fn row(&self) -> &ItemProfileRow { &self.row }

    fn display_name(&self) -> Cow<'_, str>{
        self.row.profile
            .as_ref()
            .map(|p| p.display_name.trim())
            .map(|n| if n.is_empty() { None } else { Some (n) })
            .flatten()
            .map(|n| n.into())
            // TODO: Detect/protect against someone setting a userID that mimics a pubkey?
            .unwrap_or_else(|| self.row.item.user.to_base58().into())
    }
}


struct UserPageItem {
    row: ItemRow,
    item: Item,
}

fn display_by_default(item: &Item) -> bool {
    let item_type = match &item.item_type {
        // Don't display items we can't find a type for. (newer than this server knows about):
        None => return false,
        Some(t) => t,
    };

    use crate::protos::Item_oneof_item_type as ItemType;
    match item_type {
        ItemType::post(_) => true,
        ItemType::profile(_) => false,
    }
}

impl UserPageItem {
    // TODO: Why did I (have to?) make getters for these?
    fn row(&self) -> &ItemRow { &self.row }
    fn item(&self) -> &Item { &self.item }
}

/// Represents an item of navigation on the page.
enum Nav {
    Text(String),
    Link{
        text: String,
        href: String,
    },
}


/// A type implementing ResponseError that can hold any kind of std::error::Error.
#[derive(Debug)]
struct Error {
    inner: Box<dyn std::error::Error + 'static>
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> std::result::Result<(), fmt::Error> { 
        self.inner.fmt(formatter)
    }
}

impl actix_web::error::ResponseError for Error {}

impl <E> From<E> for Error
where E: std::error::Error + 'static
{
    fn from(err: E) -> Self {
        Error{
            inner: err.into()
        }
    }
}