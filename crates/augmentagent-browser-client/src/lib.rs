//! Headless-browser client + cookie-jar helper. Drives a managed Chromium
//! over CDP for the channels that have to fall back to DOM automation
//! (LinkedIn polls/articles, Twitter when GraphQL queryIds rotate, etc.).

pub mod cdp;
pub mod cookies;
pub mod session;
