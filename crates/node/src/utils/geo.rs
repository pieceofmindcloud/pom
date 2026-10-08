//! Offline database of countries, states and cities.
//!
//! The data is the unmodified `csv-cities.csv.gz` export of the Countries
//! States Cities Database (ODbL 1.0, see `assets/geo/README.md`), embedded in
//! the binary. Nothing here reaches the network: a network's country and city
//! are always a row of this file, never text typed by the operator, and the
//! city nearest to a coordinate is answered locally.
//!
//! The file is decoded once, on the first lookup, and only the columns the
//! product reads are kept in memory.

use crate::config::{LocationPrecision, NetworkLocation, NetworkSettings};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

/// The release asset, byte for byte. `RELEASE` and `SHA256` pin which one.
static CITIES_CSV_GZ: &[u8] = include_bytes!("../../../../assets/geo/csv-cities.csv.gz");

pub const RELEASE: &str = "v3.2-export.31";
#[cfg(test)]
const SHA256: &str = "8d72a3f8c30edf237e37c20a90c14b1a4da498835a2acde6c3b6b1bbd2e39baa";

/// Side of the grid an approximate location is snapped to, in degrees: about
/// 11 km of latitude.
const APPROXIMATE_GRID_DEGREES: f64 = 0.1;
/// What an approximate location declares as its accuracy.
const APPROXIMATE_ACCURACY_M: f64 = 10_000.0;
/// A pin farther than this from the chosen city does not belong to it.
const MAX_PIN_DISTANCE_KM: f64 = 200.0;

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 50;

struct City {
    id: u32,
    name: Box<str>,
    /// `name` folded for search.
    folded: Box<str>,
    state: u32,
    country: u16,
    latitude: f64,
    longitude: f64,
}

struct Country {
    code: Box<str>,
    name: Box<str>,
    /// Indexes into `cities`, ordered by name.
    cities: Vec<u32>,
}

pub struct GeoDatabase {
    cities: Vec<City>,
    by_id: HashMap<u32, u32>,
    states: Vec<Box<str>>,
    /// Ordered by name.
    countries: Vec<Country>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CountryView {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CityView {
    pub id: u32,
    pub name: String,
    pub state: String,
    pub country_code: String,
    pub country_name: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Distance from the asked coordinate; only on a `nearest` answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_km: Option<f64>,
}

/// The embedded database, decoded on first use.
pub fn database() -> Result<&'static GeoDatabase, String> {
    static DATABASE: OnceLock<Result<GeoDatabase, String>> = OnceLock::new();
    DATABASE
        .get_or_init(|| GeoDatabase::decode(CITIES_CSV_GZ))
        .as_ref()
        .map_err(Clone::clone)
}

impl GeoDatabase {
    fn decode(compressed: &[u8]) -> Result<Self, String> {
        let mut text = String::new();
        flate2::read::GzDecoder::new(compressed)
            .read_to_string(&mut text)
            .map_err(|error| format!("descompactar a base de cidades: {error}"))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut records = CsvRecords::new(text);
        let header = records
            .next()
            .ok_or_else(|| "a base de cidades está vazia".to_string())?;
        let column = |name: &str| {
            header
                .iter()
                .position(|field| field == name)
                .ok_or_else(|| format!("a base de cidades não tem a coluna {name}"))
        };
        let (id, name, state_name, country_code, country_name, latitude, longitude) = (
            column("id")?,
            column("name")?,
            column("state_name")?,
            column("country_code")?,
            column("country_name")?,
            column("latitude")?,
            column("longitude")?,
        );
        let width = header.len();

        let mut cities = Vec::new();
        let mut by_id = HashMap::new();
        let mut states: Vec<Box<str>> = Vec::new();
        let mut state_index: HashMap<String, u32> = HashMap::new();
        let mut countries: Vec<Country> = Vec::new();
        let mut country_index: HashMap<String, u16> = HashMap::new();
        for record in records {
            if record.len() != width {
                continue;
            }
            let (Ok(city_id), Ok(lat), Ok(lon)) = (
                record[id].parse::<u32>(),
                record[latitude].parse::<f64>(),
                record[longitude].parse::<f64>(),
            ) else {
                continue;
            };
            let code = record[country_code].trim().to_ascii_uppercase();
            let city_name = record[name].trim();
            if code.len() != 2 || city_name.is_empty() || !valid_coordinate(lat, lon) {
                continue;
            }
            let country = *country_index.entry(code.clone()).or_insert_with(|| {
                countries.push(Country {
                    code: code.clone().into(),
                    name: record[country_name].trim().into(),
                    cities: Vec::new(),
                });
                (countries.len() - 1) as u16
            });
            // The same state name exists in more than one country.
            let state_key = format!("{code}\u{1f}{}", record[state_name].trim());
            let state = *state_index.entry(state_key).or_insert_with(|| {
                states.push(record[state_name].trim().into());
                (states.len() - 1) as u32
            });
            let index = cities.len() as u32;
            if by_id.insert(city_id, index).is_some() {
                continue;
            }
            countries[country as usize].cities.push(index);
            cities.push(City {
                id: city_id,
                name: city_name.into(),
                folded: fold(city_name).into(),
                state,
                country,
                latitude: lat,
                longitude: lon,
            });
        }
        if cities.is_empty() {
            return Err("a base de cidades não tem nenhuma cidade válida".into());
        }
        for country in &mut countries {
            country.cities.sort_by(|left, right| {
                let (left, right) = (&cities[*left as usize], &cities[*right as usize]);
                left.folded
                    .cmp(&right.folded)
                    .then_with(|| states[left.state as usize].cmp(&states[right.state as usize]))
            });
        }
        countries.sort_by(|left, right| left.name.cmp(&right.name));
        // `City::country` indexes the unsorted list: rewrite it.
        let position: HashMap<&str, u16> = countries
            .iter()
            .enumerate()
            .map(|(index, country)| (&*country.code, index as u16))
            .collect();
        let old_codes: HashMap<u16, String> = country_index
            .into_iter()
            .map(|(code, index)| (index, code))
            .collect();
        for city in &mut cities {
            city.country = position[old_codes[&city.country].as_str()];
        }
        Ok(Self {
            cities,
            by_id,
            states,
            countries,
        })
    }

    fn view(&self, city: &City, distance_km: Option<f64>) -> CityView {
        let country = &self.countries[city.country as usize];
        CityView {
            id: city.id,
            name: city.name.to_string(),
            state: self.states[city.state as usize].to_string(),
            country_code: country.code.to_string(),
            country_name: country.name.to_string(),
            latitude: city.latitude,
            longitude: city.longitude,
            distance_km,
        }
    }

    pub fn countries(&self) -> Vec<CountryView> {
        self.countries
            .iter()
            .map(|country| CountryView {
                code: country.code.to_string(),
                name: country.name.to_string(),
            })
            .collect()
    }

    pub fn city(&self, id: u32) -> Option<CityView> {
        let index = *self.by_id.get(&id)?;
        Some(self.view(&self.cities[index as usize], None))
    }

    /// Cities of a country whose name matches `query`, names that start with
    /// it first. An empty query lists the country in name order.
    pub fn search(&self, country_code: &str, query: &str, limit: Option<usize>) -> Vec<CityView> {
        let limit = clamp_limit(limit);
        let code = country_code.trim().to_ascii_uppercase();
        let Some(country) = self.countries.iter().find(|country| *country.code == *code) else {
            return Vec::new();
        };
        let query = fold(query.trim());
        let cities = country.cities.iter().map(|index| &self.cities[*index as usize]);
        let mut matches: Vec<&City> = cities
            .clone()
            .filter(|city| city.folded.starts_with(&query))
            .take(limit)
            .collect();
        if matches.len() < limit && !query.is_empty() {
            let rest = limit - matches.len();
            matches.extend(
                cities
                    .filter(|city| !city.folded.starts_with(&query) && city.folded.contains(&query))
                    .take(rest),
            );
        }
        matches.into_iter().map(|city| self.view(city, None)).collect()
    }

    /// The cities closest to a coordinate, nearest first. The distance is to
    /// the city's reference point, not to its boundary.
    pub fn nearest(&self, latitude: f64, longitude: f64, limit: Option<usize>) -> Vec<CityView> {
        if !valid_coordinate(latitude, longitude) {
            return Vec::new();
        }
        let limit = clamp_limit(limit);
        let mut ranked: Vec<(f64, &City)> = self
            .cities
            .iter()
            .map(|city| {
                (
                    distance_km(latitude, longitude, city.latitude, city.longitude),
                    city,
                )
            })
            .collect();
        let keep = limit.min(ranked.len());
        ranked.select_nth_unstable_by(keep - 1, |left, right| left.0.total_cmp(&right.0));
        ranked.truncate(keep);
        ranked.sort_by(|left, right| left.0.total_cmp(&right.0));
        ranked
            .into_iter()
            .map(|(distance, city)| self.view(city, Some((distance * 10.0).round() / 10.0)))
            .collect()
    }
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn valid_coordinate(latitude: f64, longitude: f64) -> bool {
    latitude.is_finite()
        && longitude.is_finite()
        && (-90.0..=90.0).contains(&latitude)
        && (-180.0..=180.0).contains(&longitude)
}

/// Great-circle distance between two coordinates.
pub fn distance_km(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let (phi_a, phi_b) = (lat_a.to_radians(), lat_b.to_radians());
    let d_phi = (lat_b - lat_a).to_radians();
    let d_lambda = (lon_b - lon_a).to_radians();
    let a = (d_phi / 2.0).sin().powi(2) + phi_a.cos() * phi_b.cos() * (d_lambda / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().min(1.0).asin()
}

/// Lower case without the common Latin diacritics, so "sao" finds "São Paulo".
fn fold(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| match character {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => 'c',
            'ď' | 'đ' => 'd',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
            'ğ' | 'ģ' => 'g',
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => 'i',
            'ķ' => 'k',
            'ł' | 'ľ' | 'ļ' => 'l',
            'ñ' | 'ń' | 'ň' | 'ņ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
            'ř' => 'r',
            'ś' | 'š' | 'ş' | 'ș' => 's',
            'ť' | 'ț' | 'ţ' => 't',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => 'u',
            'ý' | 'ÿ' => 'y',
            'ź' | 'ž' | 'ż' => 'z',
            other => other,
        })
        .collect()
}

/// Makes a network's location what the database and the chosen precision say.
///
/// The country, state and city names are rewritten from the city's row, so
/// they are never the operator's text. `settings.location` arrives as the pin
/// the operator placed and leaves as the point the network publishes:
///
/// - `city`: the city's reference point, whatever the pin;
/// - `approximate`: the pin snapped to a grid of about 10 km;
/// - `exact`: the pin.
///
/// Without a pin the city's reference point is published. The result is
/// stable: applying it again to its own output changes nothing, which is what
/// lets every member apply the settings the principal sends.
pub fn apply_location(settings: &mut NetworkSettings) -> Result<(), String> {
    let Some(city_id) = settings.city_id else {
        settings.country_code.clear();
        settings.country_name.clear();
        settings.state.clear();
        settings.city.clear();
        settings.location = None;
        return Ok(());
    };
    let city = database()?
        .city(city_id)
        .ok_or_else(|| "a cidade escolhida não existe na base de cidades".to_string())?;
    let pin = settings.location.take();
    if let Some(pin) = &pin {
        if !valid_coordinate(pin.latitude, pin.longitude) {
            return Err("as coordenadas da rede são inválidas".into());
        }
        let distance = distance_km(pin.latitude, pin.longitude, city.latitude, city.longitude);
        if distance > MAX_PIN_DISTANCE_KM {
            return Err(format!(
                "o ponto escolhido fica a {distance:.0} km de {}; escolha a cidade mais próxima dele",
                city.name
            ));
        }
    }
    let centre = NetworkLocation {
        latitude: city.latitude,
        longitude: city.longitude,
        accuracy_m: None,
    };
    settings.location = Some(match (settings.location_precision, pin) {
        (LocationPrecision::City, _) | (_, None) => centre,
        (LocationPrecision::Approximate, Some(pin)) => NetworkLocation {
            latitude: snap(pin.latitude),
            longitude: snap(pin.longitude),
            accuracy_m: Some(APPROXIMATE_ACCURACY_M),
        },
        (LocationPrecision::Exact, Some(pin)) => NetworkLocation {
            latitude: pin.latitude,
            longitude: pin.longitude,
            accuracy_m: pin
                .accuracy_m
                .filter(|accuracy| accuracy.is_finite() && *accuracy >= 0.0),
        },
    });
    settings.country_code = city.country_code;
    settings.country_name = city.country_name;
    settings.state = city.state;
    settings.city = city.name;
    Ok(())
}

/// A city of the embedded database, for tests that need a valid location.
#[cfg(test)]
pub fn test_city_id() -> u32 {
    database().expect("embedded database").nearest(-25.43, -49.27, Some(1))[0].id
}

fn snap(degrees: f64) -> f64 {
    let steps = 1.0 / APPROXIMATE_GRID_DEGREES;
    (degrees * steps).round() / steps
}

/// Records of an RFC 4180 file: quoted fields, `""` as a quote, and line
/// breaks inside quotes.
struct CsvRecords<'a> {
    rest: std::str::Chars<'a>,
    done: bool,
}

impl<'a> CsvRecords<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            rest: text.chars(),
            done: false,
        }
    }
}

impl Iterator for CsvRecords<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        if self.done {
            return None;
        }
        let mut record = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut any = false;
        loop {
            let Some(character) = self.rest.next() else {
                self.done = true;
                if !any {
                    return None;
                }
                record.push(field);
                return Some(record);
            };
            any = true;
            if quoted {
                if character != '"' {
                    field.push(character);
                } else if self.rest.clone().next() == Some('"') {
                    self.rest.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
                continue;
            }
            match character {
                '"' => quoted = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    record.push(field);
                    return Some(record);
                }
                other => field.push(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "id,name,state_name,country_code,country_name,latitude,longitude\n\
        1,Curitiba,Paraná,BR,Brazil,-25.42778,-49.27306\n\
        2,São José dos Pinhais,Paraná,BR,Brazil,-25.53472,-49.20639\n\
        3,\"Paranaguá, \"\"the port\"\"\",Paraná,BR,Brazil,-25.52,-48.50944\n\
        4,Lisbon,Lisbon,PT,Portugal,38.72635,-9.14843\n\
        5,Nowhere,,XX,Broken,200,0\n";

    fn sample() -> GeoDatabase {
        GeoDatabase::parse(SAMPLE).expect("sample database")
    }

    fn settings(city_id: Option<u32>, precision: LocationPrecision) -> NetworkSettings {
        NetworkSettings {
            city_id,
            location_precision: precision,
            ..Default::default()
        }
    }

    #[test]
    fn the_embedded_file_is_the_pinned_release() {
        use sha2::Digest;
        assert_eq!(hex::encode(sha2::Sha256::digest(CITIES_CSV_GZ)), SHA256);
    }

    #[test]
    fn the_embedded_database_answers_the_city_under_a_coordinate() {
        let database = database().expect("embedded database");
        assert!(database.cities.len() > 100_000);
        let nearest = database.nearest(-25.43, -49.27, Some(3));
        assert_eq!(nearest[0].name, "Curitiba");
        assert_eq!(nearest[0].country_code, "BR");
        assert_eq!(nearest[0].state, "Paraná");
        assert!(nearest[0].distance_km.unwrap() < 5.0);
        assert!(nearest.windows(2).all(|pair| pair[0].distance_km <= pair[1].distance_km));
        assert_eq!(database.city(nearest[0].id).unwrap().name, "Curitiba");
    }

    #[test]
    fn quoted_fields_and_broken_rows_are_read_as_the_file_means() {
        let database = sample();
        assert_eq!(database.cities.len(), 4);
        assert_eq!(database.city(3).unwrap().name, "Paranaguá, \"the port\"");
        assert!(database.city(5).is_none());
        let countries = database.countries();
        assert_eq!(countries[0].name, "Brazil");
        assert_eq!(countries[1].code, "PT");
        assert_eq!(database.city(4).unwrap().country_name, "Portugal");
    }

    #[test]
    fn a_city_is_found_by_typing_it_without_accents() {
        let database = sample();
        let found = database.search("br", "sao", None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "São José dos Pinhais");
        // Names that start with the text come before names that contain it.
        let found = database.search("BR", "p", None);
        assert_eq!(found[0].id, 3);
        assert_eq!(found[1].id, 2);
        assert_eq!(database.search("BR", "", None).len(), 3);
        assert!(database.search("ZZ", "", None).is_empty());
    }

    #[test]
    fn each_precision_publishes_what_it_promises() {
        let database = database().expect("embedded database");
        let curitiba = database.nearest(-25.43, -49.27, Some(1)).remove(0);
        let pin = NetworkLocation {
            latitude: -25.4412,
            longitude: -49.2337,
            accuracy_m: Some(12.0),
        };

        let mut exact = settings(Some(curitiba.id), LocationPrecision::Exact);
        exact.location = Some(pin.clone());
        exact.city = "typed by hand".into();
        apply_location(&mut exact).unwrap();
        assert_eq!(exact.location, Some(pin.clone()));
        assert_eq!(exact.city, "Curitiba");
        assert_eq!(exact.country_code, "BR");
        assert_eq!(exact.state, "Paraná");

        let mut approximate = settings(Some(curitiba.id), LocationPrecision::Approximate);
        approximate.location = Some(pin.clone());
        apply_location(&mut approximate).unwrap();
        let published = approximate.location.clone().unwrap();
        assert_eq!((published.latitude, published.longitude), (-25.4, -49.2));
        assert_eq!(published.accuracy_m, Some(APPROXIMATE_ACCURACY_M));
        // A member applying what the principal sent publishes the same point.
        let before = approximate.clone();
        apply_location(&mut approximate).unwrap();
        assert_eq!(approximate, before);

        let mut city = settings(Some(curitiba.id), LocationPrecision::City);
        city.location = Some(pin);
        apply_location(&mut city).unwrap();
        let published = city.location.unwrap();
        assert_eq!(
            (published.latitude, published.longitude, published.accuracy_m),
            (curitiba.latitude, curitiba.longitude, None)
        );
    }

    #[test]
    fn a_location_outside_the_database_is_refused() {
        let database = database().expect("embedded database");
        let curitiba = database.nearest(-25.43, -49.27, Some(1)).remove(0);

        let mut unknown = settings(Some(u32::MAX), LocationPrecision::Exact);
        assert!(apply_location(&mut unknown).is_err());

        let mut far = settings(Some(curitiba.id), LocationPrecision::Exact);
        far.location = Some(NetworkLocation {
            latitude: 38.72,
            longitude: -9.14,
            accuracy_m: None,
        });
        assert!(apply_location(&mut far).is_err());

        let mut none = settings(None, LocationPrecision::Exact);
        none.country_code = "BR".into();
        none.city = "typed by hand".into();
        none.location = Some(NetworkLocation {
            latitude: 1.0,
            longitude: 1.0,
            accuracy_m: None,
        });
        apply_location(&mut none).unwrap();
        assert!(none.city.is_empty() && none.country_code.is_empty() && none.location.is_none());
    }
}
