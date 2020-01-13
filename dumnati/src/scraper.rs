use crate::{graph, metadata};
use actix::prelude::*;
use failure::{Error, Fallible};
use futures::future;
use futures::prelude::*;
use reqwest::Method;

/// Release scraper.
#[derive(Clone, Debug)]
pub struct Scraper {
    graph: graph::Graph,
    hclient: reqwest::r#async::Client,
    stream: String,
    stream_metadata_url: reqwest::Url,
    release_index_url: reqwest::Url,
}

impl Scraper {
    pub fn new<S>(stream: S) -> Fallible<Self>
    where
        S: Into<String>,
    {
        let stream = stream.into();
        let vars = hashmap! { "stream".to_string() => stream.clone() };
        let releases_json = envsubst::substitute(metadata::RELEASES_JSON, &vars)?;
        let stream_json = envsubst::substitute(metadata::STREAM_JSON, &vars)?;
        let scraper = Self {
            graph: graph::Graph::default(),
            hclient: reqwest::r#async::ClientBuilder::new().build()?,
            stream,
            release_index_url: reqwest::Url::parse(&releases_json)?,
            stream_metadata_url: reqwest::Url::parse(&stream_json)?,
        };
        Ok(scraper)
    }

    /// Return a request builder with base URL and parameters set.
    fn new_request(
        &self,
        method: reqwest::Method,
        url: reqwest::Url,
    ) -> Fallible<reqwest::r#async::RequestBuilder> {
        let builder = self.hclient.request(method, url);
        Ok(builder)
    }

    /// Fetch releases from release-index.
    fn fetch_releases(&self) -> impl Future<Item = Vec<metadata::Release>, Error = Error> {
        let url = self.release_index_url.clone();
        let req = self.new_request(Method::GET, url);
        future::result(req)
            .and_then(|req| req.send().from_err())
            .and_then(|resp| resp.error_for_status().map_err(Error::from))
            .and_then(|mut resp| resp.json::<metadata::ReleasesJSON>().from_err())
            .map(|json| json.releases)
    }

    /// Fetch updates metadata.
    fn fetch_updates(&self) -> impl Future<Item = metadata::UpdatesJSON, Error = Error> {
        let url = self.stream_metadata_url.clone();
        let req = self.new_request(Method::GET, url);
        future::result(req)
            .and_then(|req| req.send().from_err())
            .and_then(|resp| resp.error_for_status().map_err(Error::from))
            .and_then(|mut resp| resp.json::<metadata::UpdatesJSON>().from_err())
    }

    /// Combine release-index and updates metadata.
    fn assemble_graph(&self) -> impl Future<Item = graph::Graph, Error = Error> {
        let stream_updates = self.fetch_updates();
        let stream_releases = self.fetch_releases();

        let updates = stream_releases
            .join(stream_updates)
            .and_then(|(graph, updates)| graph::Graph::from_metadata(graph, updates));
        updates
    }
}

impl Actor for Scraper {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Kick-start the state machine.
        Self::tick_now(ctx);
    }
}

pub(crate) struct RefreshTick {}

impl Message for RefreshTick {
    type Result = Result<(), Error>;
}

impl Handler<RefreshTick> for Scraper {
    type Result = ResponseActFuture<Self, (), Error>;

    fn handle(&mut self, _msg: RefreshTick, _ctx: &mut Self::Context) -> Self::Result {
        crate::UPSTREAM_SCRAPES
            .with_label_values(&[&self.stream])
            .inc();

        let updates = self.assemble_graph();

        let update_graph = actix::fut::wrap_future::<_, Self>(updates)
            .map_err(|err, _actor, _ctx| log::error!("{}", err))
            .map(|graph, actor, _ctx| {
                actor.graph = graph;
                let refresh_timestamp = chrono::Utc::now();
                crate::LAST_REFRESH
                    .with_label_values(&[&actor.stream])
                    .set(refresh_timestamp.timestamp());
                crate::GRAPH_FINAL_EDGES
                    .with_label_values(&[&actor.stream])
                    .set(actor.graph.edges.len() as i64);
                crate::GRAPH_FINAL_RELEASES
                    .with_label_values(&[&actor.stream])
                    .set(actor.graph.nodes.len() as i64);
            })
            .then(|_r, _actor, ctx| {
                Self::tick_later(ctx, std::time::Duration::from_secs(30));
                actix::fut::ok(())
            });

        Box::new(update_graph)
    }
}

pub(crate) struct GetCachedGraph {
    pub(crate) stream: String,
}

impl Default for GetCachedGraph {
    fn default() -> Self {
        Self {
            stream: "testing".to_string(),
        }
    }
}

impl Message for GetCachedGraph {
    type Result = Result<graph::Graph, Error>;
}

impl Handler<GetCachedGraph> for Scraper {
    type Result = ResponseActFuture<Self, graph::Graph, Error>;
    fn handle(&mut self, msg: GetCachedGraph, _ctx: &mut Self::Context) -> Self::Result {
        use failure::format_err;
        if msg.stream != self.stream {
            return Box::new(actix::fut::err(format_err!(
                "unexpected stream '{}'",
                msg.stream
            )));
        }
        Box::new(actix::fut::ok(self.graph.clone()))
    }
}

impl Scraper {
    /// Schedule an immediate refresh of the state machine.
    pub fn tick_now(ctx: &mut Context<Self>) {
        ctx.notify(RefreshTick {})
    }

    /// Schedule a delayed refresh of the state machine.
    pub fn tick_later(ctx: &mut Context<Self>, after: std::time::Duration) -> actix::SpawnHandle {
        ctx.notify_later(RefreshTick {}, after)
    }
}
