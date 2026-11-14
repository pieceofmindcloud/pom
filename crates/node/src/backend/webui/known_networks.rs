//! Every public network this machine has heard of, kept in `pom.db`.
//!
//! Live discovery (pkarr, relays, the federated directory) answers only for
//! the networks reachable right now, and it takes seconds. The Networks
//! screen first lists what this table holds, paged and searched here, and
//! then refreshes from the live listing, which records what it finds through
//! `remember_public_networks`. The table is a cache for the screen: nothing
//! decides trust or admission from it, and every entry went through the
//! same validation as the live listing before it was recorded.

use super::*;
use crate::db::{KnownNetworkEntry, PomDb};
use crate::gateway::PublicNetworkView;

/// A network the live listing has not returned for this long leaves the
/// table. A published network is described to the relays every 30 s
/// (`network_lobby::INFO_REFRESH`), so two days without it means it is no
/// longer public, or its machine is gone; it comes back with its next listing.
/// The pkarr record cannot tell: it says the node is alive, not that its
/// network is still listed.
const KNOWN_NETWORK_MAX_AGE_SECS: i64 = 2 * 24 * 60 * 60;
/// Rows kept at most, the most recently seen.
const KNOWN_NETWORKS_MAX: usize = 5000;
const DEFAULT_PAGE_SIZE: usize = 25;
const MAX_PAGE_SIZE: usize = 100;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// What the search matches: the text a person reads on the row and in the
/// side panel, folded like the city search.
fn search_text(network: &PublicNetworkView) -> String {
    crate::geo::fold(
        &[
            network.name.as_str(),
            network.cluster_id.as_str(),
            network.organization.as_str(),
            network.description.as_str(),
            network.city.as_str(),
            network.state.as_str(),
            network.country_name.as_str(),
            network.country_code.as_str(),
        ]
        .join(" "),
    )
}

/// Records the networks a listing returned. A failure is logged and
/// otherwise ignored: the listing itself never depends on the table.
pub(crate) fn remember_public_networks(database: &PomDb, networks: &[PublicNetworkView]) {
    let entries: Vec<KnownNetworkEntry> = networks
        .iter()
        .filter(|network| !network.fingerprint.is_empty())
        .filter_map(|network| {
            Some(KnownNetworkEntry {
                fingerprint: network.fingerprint.clone(),
                view_json: serde_json::to_string(network).ok()?,
                search_text: search_text(network),
            })
        })
        .collect();
    if let Err(error) = database.remember_public_networks(
        &entries,
        unix_now(),
        KNOWN_NETWORK_MAX_AGE_SECS,
        KNOWN_NETWORKS_MAX,
    ) {
        warn!("could not record the discovered public networks: {error:#}");
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnownNetworksSort {
    #[default]
    Distance,
    Name,
    Recent,
}

#[derive(Debug, Default, Deserialize)]
pub struct KnownNetworksQuery {
    #[serde(default)]
    pub q: String,
    /// Country code, or the country name for a network listed without one.
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub sort: KnownNetworksSort,
    /// The point distances are measured from; without it `distance` sorts
    /// like `recent` and `within_km` is ignored.
    /// A value that is not a finite number (`latitude=` or `undefined` from
    /// an older screen) counts as absent instead of refusing the listing.
    #[serde(default, deserialize_with = "lenient_f64")]
    pub latitude: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub longitude: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    pub within_km: Option<f64>,
    /// 1-based.
    pub page: Option<usize>,
    pub page_size: Option<usize>,
}

fn lenient_f64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite()))
}

#[derive(Debug, Serialize)]
pub struct KnownNetwork {
    pub network: PublicNetworkView,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CountryOption {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct KnownNetworksPage {
    pub networks: Vec<KnownNetwork>,
    /// Rows matching the query, over all pages.
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    /// The countries of every known network, for the filter.
    pub countries: Vec<CountryOption>,
    /// The cities of the chosen country, or of every known network.
    pub cities: Vec<String>,
}

fn country_key(network: &PublicNetworkView) -> &str {
    if network.country_code.is_empty() {
        &network.country_name
    } else {
        &network.country_code
    }
}

/// Filters, orders and pages the known networks, leaving out `excluded`
/// fingerprints (the rows the screen pins above the list).
pub(crate) fn known_networks_page(
    database: &PomDb,
    query: &KnownNetworksQuery,
    excluded: &std::collections::HashSet<String>,
) -> anyhow::Result<KnownNetworksPage> {
    let all: Vec<KnownNetwork> = database
        .known_public_networks("")?
        .into_iter()
        .filter_map(|row| {
            Some(KnownNetwork {
                network: serde_json::from_str(&row.view_json).ok()?,
                first_seen_at: row.first_seen_at,
                last_seen_at: row.last_seen_at,
            })
        })
        .filter(|known| !excluded.contains(&known.network.fingerprint))
        .collect();

    let mut countries: Vec<CountryOption> = Vec::new();
    for known in &all {
        let code = country_key(&known.network);
        if !code.is_empty() && !countries.iter().any(|country| country.code == code) {
            let name = if known.network.country_name.is_empty() {
                code
            } else {
                &known.network.country_name
            };
            countries.push(CountryOption {
                code: code.to_string(),
                name: name.to_string(),
            });
        }
    }
    countries.sort_by(|left, right| left.name.cmp(&right.name));
    let country = query.country.trim();
    let mut cities: Vec<String> = all
        .iter()
        .filter(|known| country.is_empty() || country_key(&known.network) == country)
        .map(|known| known.network.city.clone())
        .filter(|city| !city.is_empty())
        .collect();
    cities.sort();
    cities.dedup();

    let search = crate::geo::fold(query.q.trim());
    let matching: std::collections::HashSet<String> = if search.is_empty() {
        Default::default()
    } else {
        database
            .known_public_networks(&search)?
            .into_iter()
            .filter_map(|row| {
                serde_json::from_str::<PublicNetworkView>(&row.view_json)
                    .ok()
                    .map(|network| network.fingerprint)
            })
            .collect()
    };
    let city = query.city.trim();
    let viewer = query.latitude.zip(query.longitude);
    let distance = |network: &PublicNetworkView| -> Option<f64> {
        let (latitude, longitude) = viewer?;
        let location = network.location.as_ref()?;
        Some(crate::geo::distance_km(
            latitude,
            longitude,
            location.latitude,
            location.longitude,
        ))
    };
    let within_km = query.within_km.filter(|km| *km > 0.0 && viewer.is_some());
    let mut rows: Vec<KnownNetwork> = all
        .into_iter()
        .filter(|known| search.is_empty() || matching.contains(&known.network.fingerprint))
        .filter(|known| country.is_empty() || country_key(&known.network) == country)
        .filter(|known| city.is_empty() || known.network.city == city)
        .filter(|known| {
            within_km.is_none_or(|km| distance(&known.network).is_some_and(|value| value <= km))
        })
        .collect();
    // The table already comes most recently seen first; the sorts are stable,
    // so ties keep that order.
    match query.sort {
        KnownNetworksSort::Recent => {}
        KnownNetworksSort::Name => rows.sort_by_cached_key(|known| {
            crate::geo::fold(if known.network.name.is_empty() {
                &known.network.cluster_id
            } else {
                &known.network.name
            })
        }),
        // Whatever has no distance goes last.
        KnownNetworksSort::Distance if viewer.is_some() => {
            rows.sort_by(
                |left, right| match (distance(&left.network), distance(&right.network)) {
                    (Some(left), Some(right)) => left.total_cmp(&right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                },
            )
        }
        KnownNetworksSort::Distance => {}
    }

    let total = rows.len();
    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let pages = total.div_ceil(page_size).max(1);
    let page = query.page.unwrap_or(1).clamp(1, pages);
    let networks = rows
        .into_iter()
        .skip((page - 1) * page_size)
        .take(page_size)
        .collect();
    Ok(KnownNetworksPage {
        networks,
        total,
        page,
        page_size,
        countries,
        cities,
    })
}

/// `GET /api/ui/networks/known` - a page of the networks this machine has
/// heard of, without this machine's own network and the imported ones, which
/// the screen pins above every page.
pub(super) async fn known_networks(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<KnownNetworksQuery>,
) -> Response {
    let mut excluded: std::collections::HashSet<String> =
        super::networks::custom_networks(&state.database)
            .into_iter()
            .map(|entry| entry.network.fingerprint)
            .collect();
    if let Some(own) = crate::gateway::local_network_view(&state).await {
        excluded.insert(own.fingerprint);
    }
    let database = state.database.clone();
    let page = tokio::task::spawn_blocking(move || {
        // Also here, not only after a live listing: one that fails records
        // nothing, and the stale rows would stay on the screen.
        remember_public_networks(&database, &[]);
        known_networks_page(&database, &query, &excluded)
    })
    .await;
    match page {
        Ok(Ok(page)) => {
            // The reachability column asks about these keys, and the locate
            // route only answers for publishers this node lists.
            let listed: Vec<PublicNetworkView> = page
                .networks
                .iter()
                .map(|known| known.network.clone())
                .collect();
            crate::gateway::remember_listed_publishers(&listed);
            Json(page).into_response()
        }
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{error:#}") })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coordinate_that_is_not_a_number_counts_as_absent() {
        let uri: axum::http::Uri = "/x?latitude=undefined&longitude=&within_km=50&page=1"
            .parse()
            .unwrap();
        let Query(query) = Query::<KnownNetworksQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(query.latitude, None);
        assert_eq!(query.longitude, None);
        assert_eq!(query.within_km, Some(50.0));
        assert_eq!(query.page, Some(1));
    }

    fn network(fingerprint: &str, name: &str, city: &str, latitude: f64) -> PublicNetworkView {
        PublicNetworkView {
            cluster_id: format!("cluster-{fingerprint}"),
            name: name.to_string(),
            fingerprint: fingerprint.to_string(),
            publisher_key: format!("pk:{fingerprint}"),
            relay_hints: Vec::new(),
            trust_document: String::new(),
            organization: String::new(),
            description: format!("Rede de {city}"),
            rules: String::new(),
            channels: String::new(),
            country_code: "BR".to_string(),
            country_name: "Brasil".to_string(),
            state: String::new(),
            city: city.to_string(),
            geo_label: String::new(),
            access_policy: Default::default(),
            accepted_enrollments: Default::default(),
            public_stats: false,
            lobby_chat: false,
            location: Some(crate::config::NetworkLocation {
                latitude,
                longitude: -49.0,
                accuracy_m: None,
            }),
            discovered_via: "pkarr".to_string(),
            directory_replicas: Vec::new(),
        }
    }

    fn temporary_db(label: &str) -> PomDb {
        let root = std::env::temp_dir().join(format!(
            "pom-known-networks-{label}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&root);
        PomDb::open(root.join("pom.db")).expect("open database")
    }

    fn query() -> KnownNetworksQuery {
        KnownNetworksQuery::default()
    }

    #[test]
    fn a_network_heard_again_keeps_one_row_with_its_latest_description() {
        let database = temporary_db("upsert");
        remember_public_networks(&database, &[network("a", "Alfa", "Curitiba", -25.0)]);
        let mut renamed = network("a", "Alfa Nova", "Curitiba", -25.0);
        renamed.description = "nova descrição".to_string();
        remember_public_networks(&database, &[renamed]);
        let page = known_networks_page(&database, &query(), &Default::default()).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.networks[0].network.name, "Alfa Nova");
        assert_eq!(page.networks[0].network.description, "nova descrição");
    }

    #[test]
    fn the_search_folds_accents_and_needs_every_word() {
        let database = temporary_db("search");
        remember_public_networks(
            &database,
            &[
                network("a", "Alfa", "São Paulo", -23.5),
                network("b", "Beta", "Curitiba", -25.4),
            ],
        );
        let search = |text: &str| {
            let query = KnownNetworksQuery {
                q: text.to_string(),
                ..query()
            };
            known_networks_page(&database, &query, &Default::default())
                .unwrap()
                .networks
                .into_iter()
                .map(|known| known.network.fingerprint)
                .collect::<Vec<_>>()
        };
        assert_eq!(search("sao paulo"), vec!["a"]);
        assert_eq!(search("BETA curitiba"), vec!["b"]);
        assert!(search("beta paulo").is_empty());
        assert!(search("100%").is_empty());
    }

    #[test]
    fn pages_are_cut_after_filtering_and_sorting_and_leave_out_pinned_rows() {
        let database = temporary_db("pages");
        let networks: Vec<PublicNetworkView> = (0..7)
            .map(|index| {
                network(
                    &format!("n{index}"),
                    &format!("Rede {index}"),
                    "Curitiba",
                    -20.0 - index as f64,
                )
            })
            .collect();
        remember_public_networks(&database, &networks);
        let excluded = std::collections::HashSet::from(["n0".to_string()]);
        let page = |number: usize| {
            let query = KnownNetworksQuery {
                sort: KnownNetworksSort::Distance,
                latitude: Some(-30.0),
                longitude: Some(-49.0),
                page: Some(number),
                page_size: Some(4),
                ..query()
            };
            known_networks_page(&database, &query, &excluded).unwrap()
        };
        let first = page(1);
        assert_eq!(first.total, 6);
        let names: Vec<_> = first
            .networks
            .iter()
            .map(|known| known.network.fingerprint.clone())
            .collect();
        assert_eq!(names, vec!["n6", "n5", "n4", "n3"]);
        let second = page(2);
        assert_eq!(second.networks.len(), 2);
        // A page past the end answers the last one.
        assert_eq!(page(9).page, 2);
    }

    #[test]
    fn the_radius_keeps_only_networks_within_it() {
        let database = temporary_db("radius");
        remember_public_networks(
            &database,
            &[
                network("near", "Perto", "A", -25.0),
                network("far", "Longe", "B", -5.0),
            ],
        );
        let query = KnownNetworksQuery {
            latitude: Some(-25.1),
            longitude: Some(-49.0),
            within_km: Some(100.0),
            ..query()
        };
        let page = known_networks_page(&database, &query, &Default::default()).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.networks[0].network.fingerprint, "near");
    }

    #[test]
    fn a_network_the_live_listing_dropped_two_days_ago_leaves_when_the_page_is_read() {
        let database = temporary_db("stale");
        let entry = |fingerprint: &str| KnownNetworkEntry {
            fingerprint: fingerprint.to_string(),
            view_json: serde_json::to_string(&network(fingerprint, fingerprint, "C", -25.0))
                .unwrap(),
            search_text: fingerprint.to_string(),
        };
        let now = unix_now();
        database
            .remember_public_networks(
                &[entry("gone")],
                now - KNOWN_NETWORK_MAX_AGE_SECS - 60,
                i64::MAX,
                10,
            )
            .unwrap();
        database
            .remember_public_networks(&[entry("recent")], now - 60 * 60, i64::MAX, 10)
            .unwrap();
        // What the page handler runs before reading: no live answer needed.
        remember_public_networks(&database, &[]);
        let page = known_networks_page(&database, &query(), &Default::default()).unwrap();
        let listed: Vec<&str> = page
            .networks
            .iter()
            .map(|known| known.network.fingerprint.as_str())
            .collect();
        assert_eq!(listed, ["recent"]);
    }

    #[test]
    fn old_rows_expire_and_the_table_keeps_the_most_recent_up_to_its_cap() {
        let database = temporary_db("evict");
        let entry = |fingerprint: &str| KnownNetworkEntry {
            fingerprint: fingerprint.to_string(),
            view_json: serde_json::to_string(&network(fingerprint, fingerprint, "C", -25.0))
                .unwrap(),
            search_text: fingerprint.to_string(),
        };
        database
            .remember_public_networks(&[entry("old")], 1_000, 100, 10)
            .unwrap();
        database
            .remember_public_networks(&[entry("a"), entry("b")], 2_000, 100, 10)
            .unwrap();
        database
            .remember_public_networks(&[entry("c")], 2_001, 100, 2)
            .unwrap();
        let rows: Vec<String> = database
            .known_public_networks("")
            .unwrap()
            .into_iter()
            .map(|row| {
                serde_json::from_str::<PublicNetworkView>(&row.view_json)
                    .unwrap()
                    .fingerprint
            })
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "c");
        assert!(!rows.contains(&"old".to_string()));
    }
}
