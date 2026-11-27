use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Record = csv::StringRecord;
type Membership = HashMap<String, Vec<usize>>;
type Polygon = Vec<Vec<Vec<f64>>>;
type Bbox = (f64, f64, f64, f64);

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Deserialize)]
struct GeoJson {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Properties,
    geometry: Geometry,
}

#[derive(Deserialize)]
struct Properties {
    #[serde(rename = "GID_2")]
    id: String,
    #[serde(rename = "NAME_2", default)]
    name: Option<String>,
    #[serde(rename = "HASC_2", default)]
    hasc: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Geometry {
    Polygon { coordinates: Polygon },
    MultiPolygon { coordinates: Vec<Polygon> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nation { England, Scotland, Wales, NorthernIreland }

fn nation(id: &str) -> Option<Nation> {
    match id.split('.').nth(1)? {
        "1" => Some(Nation::England),
        "2" => Some(Nation::NorthernIreland),
        "3" => Some(Nation::Scotland),
        "4" => Some(Nation::Wales),
        _ => None,
    }
}

struct County {
    folder: String,
    nation: Nation,
    polygons: Vec<(Bbox, Polygon)>,
}

fn slug(name: &str) -> String {
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn county_name(properties: &Properties) -> Result<String> {
    // GID disambiguates reused HASC codes (e.g. Bedford and Central Bedfordshire).
    for row in include_str!("../area_names.tsv").lines().skip(1) {
        let mut fields = row.split('\t');
        if fields.next() == Some(properties.id.as_str()) {
            let hasc = fields.next();
            if hasc != properties.hasc.as_deref() {
                return Err(invalid(format!("HASC changed for {}", properties.id)).into());
            }
            return Ok(slug(fields.next().ok_or_else(|| invalid("Missing override name"))?));
        }
    }
    properties.name.as_deref()
        .filter(|name| !name.trim().is_empty() && !name.eq_ignore_ascii_case("NA"))
        .map(slug)
        .filter(|name| !name.is_empty() && name != "other")
        .ok_or_else(|| invalid(format!("Missing name for {} ({:?})", properties.id, properties.hasc)).into())
}

fn bbox(polygon: &Polygon) -> Bbox {
    let mut result = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for point in polygon.iter().flatten() {
        result.0 = result.0.min(point[0]);
        result.1 = result.1.max(point[0]);
        result.2 = result.2.min(point[1]);
        result.3 = result.3.max(point[1]);
    }
    result
}

fn point_in_ring(x: f64, y: f64, ring: &[Vec<f64>]) -> bool {
    let mut inside = false;
    if ring.len() < 3 {
        return false;
    }
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        // Include points on the exterior boundary. Shared borders are resolved
        // deterministically by the sorted county order.
        let cross = (x - xi) * (yj - yi) - (y - yi) * (xj - xi);
        if cross.abs() <= 1e-12
            && x >= xi.min(xj) && x <= xi.max(xj)
            && y >= yi.min(yj) && y <= yi.max(yj)
        {
            return true;
        }
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn county_at(counties: &[County], lon: f64, lat: f64) -> Option<usize> {
    counties.iter().position(|county| {
        county.polygons.iter().any(|(bounds, polygon)| {
            let (min_x, max_x, min_y, max_y) = *bounds;
            lon >= min_x && lon <= max_x && lat >= min_y && lat <= max_y
                && polygon.first().is_some_and(|ring| point_in_ring(lon, lat, ring))
                && !polygon.iter().skip(1).any(|hole| point_in_ring(lon, lat, hole))
        })
    })
}

fn load_counties(path: &Path) -> Result<Vec<County>> {
    let data: GeoJson = serde_json::from_reader(io::BufReader::new(fs::File::open(path)?))?;
    let mut counties: Vec<County> = Vec::new();
    let mut folders: HashMap<String, usize> = HashMap::new();
    for feature in data.features {
        let country = nation(&feature.properties.id)
            .ok_or_else(|| invalid(format!("Unknown nation for {}", feature.properties.id)))?;
        // Validate NI names too, although NI is outside the Great Britain feed.
        let mut folder = county_name(&feature.properties)?;
        if country == Nation::NorthernIreland { continue; }
        // These ten GADM areas are the majority-overlapping areas identified
        // against the supplied greater_manchester.geojson boundary.
        if country == Nation::England && [
            "GBR.1.7_1", "GBR.1.14_1", "GBR.1.54_1", "GBR.1.68_1",
            "GBR.1.76_1", "GBR.1.80_1", "GBR.1.93_1", "GBR.1.100_1",
            "GBR.1.104_1", "GBR.1.111_1",
        ].contains(&feature.properties.id.as_str()) {
            folder = "greater_manchester".to_owned();
        }
        let polygons = match feature.geometry {
            Geometry::Polygon { coordinates } => vec![coordinates],
            Geometry::MultiPolygon { coordinates } => coordinates,
        };
        let polygons = polygons.into_iter().map(|polygon| (bbox(&polygon), polygon));
        if let Some(&index) = folders.get(&folder) {
            if counties[index].nation != country {
                return Err(invalid(format!("Conflicting nations for {folder}")).into());
            }
            counties[index].polygons.extend(polygons);
        } else {
            folders.insert(folder.clone(), counties.len());
            counties.push(County { folder, nation: country, polygons: polygons.collect() });
        }
    }
    if counties.is_empty() {
        return Err(invalid("No Great Britain boundaries found").into());
    }
    counties.sort_by(|a, b| a.folder.cmp(&b.folder));
    Ok(counties)
}

// An operator qualifies only when every scheduled stop call is located in
// one nation and at least three distinct local authorities are served.
const NATIONAL_MIN_AREAS: usize = 3;

const COACH_FEEDS: [(&str, &str, &str); 3] = [
    ("OP5050", "NATX", "national_express"),
    ("OP5051", "FLIX", "flixbus"),
    ("OP755", "BYCO", "berrys_coaches"),
];

fn coach_feed(id: &str, noc: &str) -> Option<usize> {
    COACH_FEEDS.iter().position(|&(agency, code, _)| {
        id == agency || noc.trim().eq_ignore_ascii_case(code)
    })
}
#[derive(Default)]
struct AgencyCoverage {
    areas: HashSet<usize>,
    unknown: bool,
}
fn national_destination(coverage: &AgencyCoverage, nations: &[Nation], wales: usize, scotland: usize) -> Option<usize> {
    if coverage.unknown || coverage.areas.len() < NATIONAL_MIN_AREAS {
        return None;
    }
    if coverage.areas.iter().all(|&i| nations[i] == Nation::Wales) {
        Some(wales)
    } else if coverage.areas.iter().all(|&i| nations[i] == Nation::Scotland) {
        Some(scotland)
    } else {
        None
    }
}

struct Table {
    reader: csv::Reader<fs::File>,
    header: Record,
}

impl Table {
    fn open(input: &Path, filename: &str) -> Result<Self> {
        let mut reader = csv::Reader::from_path(input.join(filename))?;
        let header = reader.headers()?.clone();
        Ok(Self { reader, header })
    }

    fn optional(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|column| column == name)
    }

    fn column(&self, name: &str) -> Result<usize> {
        self.optional(name).ok_or_else(|| invalid(format!("Missing GTFS column: {name}")).into())
    }
}

fn field(row: &Record, index: Option<usize>) -> &str {
    index.and_then(|index| row.get(index)).unwrap_or("")
}

// Reuse destination buffers and CSV records; never load stop_times or shapes into RAM.
struct Output {
    root: PathBuf,
    names: Vec<String>,
    active: Vec<bool>,
}

impl Output {
    fn write(
        &self,
        filename: &str,
        mut table: Table,
        mut select: impl FnMut(&Record, &mut Vec<usize>) -> Result<()>,
    ) -> Result<()> {
        let mut writers = Vec::with_capacity(self.names.len());
        for (index, name) in self.names.iter().enumerate() {
            if self.active[index] {
                let mut writer = csv::Writer::from_path(self.root.join(name).join(filename))?;
                writer.write_record(&table.header)?;
                writers.push(Some(writer));
            } else {
                writers.push(None);
            }
        }
        let mut record = Record::new();
        let mut destinations = Vec::new();
        while table.reader.read_record(&mut record)? {
            destinations.clear();
            select(&record, &mut destinations)?;
            for &index in &destinations {
                writers[index].as_mut().expect("active destination").write_record(&record)?;
            }
        }
        for writer in writers.iter_mut().flatten() {
            writer.flush()?;
        }
        Ok(())
    }

    fn membership(
        &self, input: &Path, filename: &str, key: &str, needed: &Membership, required: bool,
    ) -> Result<()> {
        if !required && !input.join(filename).exists() {
            return Ok(());
        }
        let table = Table::open(input, filename)?;
        let index = table.column(key)?;
        self.write(filename, table, |row, dest| {
            if let Some(folders) = needed.get(&row[index]) {
                dest.extend_from_slice(folders);
            }
            Ok(())
        })
    }
}

fn add_member(map: &mut Membership, id: &str, folder: usize) -> bool {
    if let Some(folders) = map.get_mut(id) {
        if folders.contains(&folder) {
            return false;
        }
        folders.push(folder);
    } else {
        map.insert(id.to_owned(), vec![folder]);
    }
    true
}

fn has_member(map: &Membership, id: &str, folder: usize) -> bool {
    map.get(id).is_some_and(|folders| folders.contains(&folder))
}

struct Stop {
    county: Option<usize>,
    parent: Option<String>,
}

fn stop_county<'a>(id: &'a str, stops: &'a HashMap<String, Stop>) -> Option<usize> {
    let mut current = id;
    let mut seen = HashSet::new();
    while let Some(stop) = stops.get(current) {
        if let Some(county) = stop.county {
            return Some(county);
        }
        if !seen.insert(current) {
            break;
        }
        current = stop.parent.as_deref()?;
    }
    None
}

fn dominant_county(counts: &HashMap<usize, u64>, other: usize) -> usize {
    // County indices are sorted by output name. Ties must be reproducible.
    counts.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map_or(other, |(&county, _)| county)
}

fn split(input: &Path, boundaries: &Path, root: &Path) -> Result<()> {
    let counties = load_counties(boundaries)?;
    println!("Loaded {} Great Britain output areas", counties.len());
    let nations: Vec<_> = counties.iter().map(|county| county.nation).collect();
    let mut names: Vec<_> = counties.iter().map(|county| county.folder.clone()).collect();
    let other = names.len();
    names.push("other".into());
    let wales = names.len();
    names.push("wales".into());
    let scotland = names.len();
    names.push("scotland".into());
    let coach_start = names.len();
    names.extend(COACH_FEEDS.iter().map(|&(_, _, name)| name.to_owned()));
    if names.iter().collect::<HashSet<_>>().len() != names.len() {
        return Err(invalid("Duplicate or reserved output name").into());
    }

    let mut stops = HashMap::new();
    {
        let mut table = Table::open(input, "stops.txt")?;
        let id = table.column("stop_id")?;
        let lon = table.column("stop_lon")?;
        let lat = table.column("stop_lat")?;
        let parent = table.optional("parent_station");
        let mut row = Record::new();
        while table.reader.read_record(&mut row)? {
            let county = match (row[lon].parse::<f64>(), row[lat].parse::<f64>()) {
                (Ok(lon), Ok(lat)) => county_at(&counties, lon, lat),
                _ => None,
            };
            let parent = field(&row, parent);
            stops.insert(row[id].to_owned(), Stop {
                county,
                parent: (!parent.is_empty()).then(|| parent.to_owned()),
            });
        }
    }
    drop(counties);
    let stop_counties: HashMap<&str, usize> = stops.keys().filter_map(|id| {
        stop_county(id, &stops).map(|county| (id.as_str(), county))
    }).collect();
    println!("Matched {} of {} stops to Great Britain boundaries", stop_counties.len(), stops.len());

    let mut agency_ids = HashSet::new();
    let mut coach_agencies = HashMap::new();
    {
        let mut table = Table::open(input, "agency.txt")?;
        let id = table.optional("agency_id");
        let noc = table.optional("agency_noc");
        let mut row = Record::new();
        while table.reader.read_record(&mut row)? {
            let id = field(&row, id);
            if let Some(index) = coach_feed(id, field(&row, noc)) {
                coach_agencies.insert(id.to_owned(), coach_start + index);
            }
            if !agency_ids.insert(id.to_owned()) {
                return Err(invalid(format!("Duplicate agency_id: {id}")).into());
            }
        }
    }
    let sole_agency = if agency_ids.len() == 1 { agency_ids.iter().next() } else { None };
    let mut routes = HashMap::new();
    let mut route_agencies = Vec::new();
    {
        let mut table = Table::open(input, "routes.txt")?;
        let id = table.column("route_id")?;
        let agency = table.optional("agency_id");
        let mut row = Record::new();
        while table.reader.read_record(&mut row)? {
            let mut agency_id = field(&row, agency);
            if agency_id.is_empty() {
                if let Some(sole) = sole_agency {
                    agency_id = sole;
                }
            }
            if !agency_ids.contains(agency_id) {
                return Err(invalid(format!("Unknown agency_id for route {}", &row[id])).into());
            }
            if routes.insert(row[id].to_owned(), route_agencies.len()).is_some() {
                return Err(invalid(format!("Duplicate route_id: {}", &row[id])).into());
            }
            route_agencies.push(agency_id.to_owned());
        }
    }

    let mut trip_routes = HashMap::new();
    let mut route_has_trips = vec![false; routes.len()];
    {
        let mut table = Table::open(input, "trips.txt")?;
        let trip = table.column("trip_id")?;
        let route = table.column("route_id")?;
        let mut row = Record::new();
        while table.reader.read_record(&mut row)? {
            let index = *routes.get(&row[route]).ok_or_else(|| {
                invalid(format!("Unknown route_id: {}", &row[route]))
            })?;
            route_has_trips[index] = true;
            if trip_routes.insert(row[trip].to_owned(), index).is_some() {
                return Err(invalid(format!("Duplicate trip_id: {}", &row[trip])).into());
            }
        }
    }

    // First pass: count all mapped stop calls for each route, not each operator.
    let mut counts = vec![HashMap::new(); routes.len()];
    let mut coverage: HashMap<&str, AgencyCoverage> = HashMap::new();
    {
        let mut table = Table::open(input, "stop_times.txt")?;
        let trip = table.column("trip_id")?;
        let stop = table.column("stop_id")?;
        let mut row = Record::new();
        let mut processed = 0u64;
        while table.reader.read_record(&mut row)? {
            let route = *trip_routes.get(&row[trip]).ok_or_else(|| {
                invalid(format!("Unknown stop_times trip_id: {}", &row[trip]))
            })?;
            if coach_agencies.contains_key(route_agencies[route].as_str()) {
                continue;
            }
            let agency = coverage.entry(route_agencies[route].as_str()).or_default();
            if let Some(&county) = stop_counties.get(&row[stop]) {
                *counts[route].entry(county).or_insert(0u64) += 1;
                agency.areas.insert(county);
            } else {
                agency.unknown = true;
            }
            processed += 1;
            if processed % 10_000_000 == 0 {
                println!("Classified {processed} stop times");
            }
        }
    }
    drop(stop_counties);

    let route_folders: Vec<Option<usize>> = counts.iter().enumerate()
        .map(|(index, counts)| route_has_trips[index].then(|| {
            coach_agencies.get(route_agencies[index].as_str()).copied()
                .or_else(|| coverage.get(route_agencies[index].as_str())
                    .and_then(|area| national_destination(area, &nations, wales, scotland)))
                .unwrap_or_else(|| dominant_county(counts, other))
        }))
        .collect();
    drop(counts);
    drop(coverage);
    let mut active = vec![false; names.len()];
    let mut agencies_needed = Membership::new();
    for (index, folder) in route_folders.iter().enumerate() {
        if let Some(folder) = *folder {
            active[folder] = true;
            add_member(&mut agencies_needed, &route_agencies[index], folder);
        }
    }
    if !active.iter().any(|&active| active) {
        return Err(invalid("Source contains no trips").into());
    }
    if root.exists() && fs::read_dir(root)?.next().is_some() {
        return Err(invalid(format!("Output directory must be empty: {}", root.display())).into());
    }
    fs::create_dir_all(root)?;
    for (index, name) in names.iter().enumerate() {
        if active[index] {
            fs::create_dir(root.join(name))?;
        }
    }
    let output = Output { root: root.to_owned(), names, active };
    let route_folder = |id: &str| routes.get(id).and_then(|&index| route_folders[index]);
    let trip_folder = |id: &str| trip_routes.get(id).and_then(|&index| route_folders[index]);

    {
        let table = Table::open(input, "agency.txt")?;
        let id = table.optional("agency_id");
        output.write("agency.txt", table, |row, dest| {
            if let Some(folders) = agencies_needed.get(field(row, id)) {
                dest.extend_from_slice(folders);
            }
            Ok(())
        })?;
    }
    {
        let table = Table::open(input, "routes.txt")?;
        let id = table.column("route_id")?;
        output.write("routes.txt", table, |row, dest| {
            dest.extend(route_folder(&row[id]));
            Ok(())
        })?;
    }

    let mut services_needed = Membership::new();
    let mut shapes_needed = Membership::new();
    {
        let table = Table::open(input, "trips.txt")?;
        let trip = table.column("trip_id")?;
        let service = table.column("service_id")?;
        let shape = table.optional("shape_id");
        output.write("trips.txt", table, |row, dest| {
            let folder = trip_folder(&row[trip]).expect("indexed trip");
            dest.push(folder);
            add_member(&mut services_needed, &row[service], folder);
            let shape = field(row, shape);
            if !shape.is_empty() {
                add_member(&mut shapes_needed, shape, folder);
            }
            Ok(())
        })?;
    }
    let mut stops_needed = Membership::new();
    {
        let table = Table::open(input, "stop_times.txt")?;
        let trip = table.column("trip_id")?;
        let stop = table.column("stop_id")?;
        output.write("stop_times.txt", table, |row, dest| {
            let folder = trip_folder(&row[trip]).expect("indexed trip");
            // Keep the entire trip, including stops across county/national borders.
            if !stops.contains_key(&row[stop]) {
                return Err(invalid(format!("Unknown stop_id: {}", &row[stop])).into());
            }
            dest.push(folder);
            add_member(&mut stops_needed, &row[stop], folder);
            Ok(())
        })?;
    }

    // Include every ancestor station, and terminate safely even on a parent cycle.
    let mut pending: Vec<_> = stops_needed.iter().flat_map(|(stop, folders)| {
        folders.iter().map(move |&folder| (stop.clone(), folder))
    }).collect();
    while let Some((stop, folder)) = pending.pop() {
        if let Some(parent) = stops.get(&stop).and_then(|stop| stop.parent.as_deref()) {
            if !stops.contains_key(parent) {
                return Err(invalid(format!("Unknown parent_station: {parent}")).into());
            }
            if add_member(&mut stops_needed, parent, folder) {
                pending.push((parent.to_owned(), folder));
            }
        }
    }

    let mut levels_needed = Membership::new();
    {
        let table = Table::open(input, "stops.txt")?;
        let id = table.column("stop_id")?;
        let level = table.optional("level_id");
        output.write("stops.txt", table, |row, dest| {
            if let Some(folders) = stops_needed.get(&row[id]) {
                dest.extend_from_slice(folders);
                let level = field(row, level);
                if !level.is_empty() {
                    for &folder in folders {
                        add_member(&mut levels_needed, level, folder);
                    }
                }
            }
            Ok(())
        })?;
    }

    if !input.join("calendar.txt").exists() && !input.join("calendar_dates.txt").exists() {
        return Err(invalid("Source contains neither calendar.txt nor calendar_dates.txt").into());
    }
    for file in ["calendar.txt", "calendar_dates.txt"] {
        output.membership(input, file, "service_id", &services_needed, false)?;
    }
    output.membership(input, "shapes.txt", "shape_id", &shapes_needed, false)?;
    output.membership(input, "levels.txt", "level_id", &levels_needed, false)?;

    if input.join("frequencies.txt").exists() {
        let table = Table::open(input, "frequencies.txt")?;
        let trip = table.column("trip_id")?;
        output.write("frequencies.txt", table, |row, dest| {
            dest.extend(trip_folder(&row[trip]));
            Ok(())
        })?;
    }

    for filename in ["transfers.txt", "pathways.txt"] {
        if !input.join(filename).exists() {
            continue;
        }
        let table = Table::open(input, filename)?;
        let stop_cols = [table.optional("from_stop_id"), table.optional("to_stop_id")];
        let route_cols = [table.optional("from_route_id"), table.optional("to_route_id")];
        let trip_cols = [table.optional("from_trip_id"), table.optional("to_trip_id")];
        output.write(filename, table, |row, dest| {
            for (folder, &active) in output.active.iter().enumerate() {
                if active
                    && stop_cols.iter().all(|&col| {
                        let id = field(row, col);
                        id.is_empty() || has_member(&stops_needed, id, folder)
                    })
                    && route_cols.iter().all(|&col| {
                        let id = field(row, col);
                        id.is_empty() || route_folder(id) == Some(folder)
                    })
                    && trip_cols.iter().all(|&col| {
                        let id = field(row, col);
                        id.is_empty() || trip_folder(id) == Some(folder)
                    })
                {
                    dest.push(folder);
                }
            }
            Ok(())
        })?;
    }
    if input.join("feed_info.txt").exists() {
        for (index, name) in output.names.iter().enumerate() {
            if output.active[index] {
                fs::copy(input.join("feed_info.txt"), output.root.join(name).join("feed_info.txt"))?;
            }
        }
    }
    println!("Wrote {} GTFS feeds (including operator feeds and other if needed)", output.active.iter().filter(|&&active| active).count());
    Ok(())
}

fn main() -> Result<()> {
    split(Path::new("gtfs_raw"), Path::new("level-2.geojson"), Path::new("gtfs_output"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_boundaries_have_unique_gb_names() {
        let counties = load_counties(Path::new("level-2.geojson")).unwrap();
        assert_eq!(counties.len(), 162);
        assert!(counties.iter().any(|county| county.folder == "greater_london"));
        assert!(counties.iter().any(|county| county.folder == "hertfordshire"));
        assert!(counties.iter().any(|county| county.folder == "greater_manchester"));
        assert!(counties.iter().any(|county| county.folder == "north_ayrshire"));
        assert!(counties.iter().any(|county| county.folder == "gwynedd"));
        assert!(!counties.iter().any(|county| county.folder.starts_with("gbr_")));
        assert!(!counties.iter().any(|county| county.nation == Nation::NorthernIreland));
        assert!(counties.iter().any(|county| county.folder == "kent"));
        assert!(counties.iter().any(|county| county.folder == "surrey"));
        assert!(!counties.iter().any(|county| county.folder == "na" || county.folder == "other"));
    }

    #[test]
    fn national_grouping_requires_three_areas_and_exclusivity() {
        let nations = [Nation::Wales, Nation::Wales, Nation::Wales,
            Nation::Scotland, Nation::Scotland, Nation::Scotland, Nation::England];
        let coverage = |areas: &[usize], unknown| AgencyCoverage {
            areas: areas.iter().copied().collect(), unknown,
        };
        assert_eq!(national_destination(&coverage(&[0, 1, 2], false), &nations, 7, 8), Some(7));
        assert_eq!(national_destination(&coverage(&[3, 4, 5], false), &nations, 7, 8), Some(8));
        assert_eq!(national_destination(&coverage(&[0, 1], false), &nations, 7, 8), None);
        assert_eq!(national_destination(&coverage(&[0, 1, 2, 6], false), &nations, 7, 8), None);
        assert_eq!(national_destination(&coverage(&[3, 4, 5], true), &nations, 7, 8), None);
    }

    #[test]
    fn ties_choose_first_sorted_county() {
        assert_eq!(dominant_county(&HashMap::from([(5, 10), (2, 10)]), 9), 2);
        assert_eq!(dominant_county(&HashMap::new(), 9), 9);
    }

    #[test]
    fn splits_operator_routes_without_cutting_trips() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("england-splitter-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let input = dir.join("input");
        fs::create_dir(&input).unwrap();
        let output = dir.join("output");
        let boundaries = dir.join("boundaries.geojson");
        fs::write(&boundaries, r#"{"features":[
            {"properties":{"GID_2":"GBR.1.901_1","NAME_2":"Alpha"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]],[[0.5,0.5],[0.8,0.5],[0.8,0.8],[0.5,0.8],[0.5,0.5]]]}},
            {"properties":{"GID_2":"GBR.1.902_1","NAME_2":"Beta"},"geometry":{"type":"MultiPolygon","coordinates":[[[[2,0],[4,0],[4,2],[2,2],[2,0]]]]}},
            {"properties":{"GID_2":"GBR.1.903_1","NAME_2":"Gamma"},"geometry":{"type":"Polygon","coordinates":[[[4,0],[6,0],[6,2],[4,2],[4,0]]]}},
            {"properties":{"GID_2":"GBR.2.901_1","NAME_2":"OutsideGB"},"geometry":{"type":"Polygon","coordinates":[[[6,0],[8,0],[8,2],[6,2],[6,0]]]}}
        ]}"#).unwrap();
        let counties = load_counties(&boundaries).unwrap();
        assert_eq!(counties.len(), 3);
        assert_eq!(county_at(&counties, 0.6, 0.6), None); // polygon hole
        assert_eq!(county_at(&counties, 2.0, 1.0), Some(0)); // deterministic border
        assert_eq!(counties[county_at(&counties, 5.0, 1.0).unwrap()].folder, "gamma");
        assert_eq!(county_at(&counties, 7.0, 1.0), None);
        let files = [
            ("agency.txt", "agency_id,agency_name,agency_noc\na,Operator,LOCAL\nOP5050,National Express,NATX\nnewflix,FlixBus,FLIX\nOP755,Berry's Coaches,BYCO\n"),
            ("routes.txt", "route_id,agency_id,route_short_name\nr1,a,1\nr2,a,2\nr3,a,3\nr4,OP5050,4\nr5,newflix,5\nr6,OP755,6\n"),
            ("trips.txt", "route_id,service_id,trip_id,shape_id\nr1,s,t1,sh\nr2,s,t2,sh\nr3,s,t3,\nr4,s,t4,sh\nr5,s,t5,\nr6,s,t6,\n"),
            ("stops.txt", "stop_id,stop_lat,stop_lon,parent_station\np,1,1,\na1,1,1,p\na2,,,p\nb,1,3,\nw,1,7,\n"),
            ("stop_times.txt", "trip_id,stop_id,stop_sequence\nt1,a1,1\nt1,a2,2\nt1,b,3\nt1,w,4\nt2,b,1\nt3,w,1\nt4,a1,1\nt4,b,2\nt4,w,3\nt5,b,1\nt6,a2,1\n"),
            ("calendar_dates.txt", "service_id,date,exception_type\ns,20261126,1\n"),
            ("shapes.txt", "shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence\nsh,1,1,1\nsh,1,3,2\n"),
            ("frequencies.txt", "trip_id,start_time,end_time,headway_secs\nt1,08:00:00,09:00:00,600\n"),
            ("transfers.txt", "from_stop_id,to_stop_id,from_route_id,to_route_id,transfer_type\nb,a1,r1,r1,2\nb,a1,r2,r1,2\n"),
        ];
        for (name, content) in files {
            fs::write(input.join(name), content).unwrap();
        }
        split(&input, &boundaries, &output).unwrap();
        let read = |folder: &str, file: &str| fs::read_to_string(output.join(folder).join(file)).unwrap();
        assert!(read("alpha", "agency.txt").contains("a,Operator"));
        assert!(read("beta", "agency.txt").contains("a,Operator"));
        assert!(read("alpha", "trips.txt").contains("r1,s,t1,sh"));
        assert!(!read("beta", "trips.txt").contains("t1"));
        assert!(read("beta", "trips.txt").contains("r2,s,t2,sh"));
        assert!(read("other", "trips.txt").contains("r3,s,t3,"));
        assert_eq!(read("alpha", "stop_times.txt").lines().count(), 5);
        assert!(read("alpha", "stops.txt").contains("w,1,7,"));
        assert!(read("alpha", "stops.txt").contains("p,1,1,"));
        assert!(read("beta", "calendar_dates.txt").contains("s,20261126,1"));
        assert_eq!(read("beta", "shapes.txt").lines().count(), 3);
        assert_eq!(read("alpha", "frequencies.txt").lines().count(), 2);
        assert_eq!(read("beta", "frequencies.txt").lines().count(), 1);
        assert_eq!(read("alpha", "transfers.txt").lines().count(), 2);
        assert!(!output.join("alpha/calendar.txt").exists());
        assert!(!output.join("wales").exists());
        assert!(read("national_express", "trips.txt").contains("r4,s,t4,sh"));
        assert_eq!(read("national_express", "stop_times.txt").lines().count(), 4);
        assert!(read("national_express", "stops.txt").contains("w,1,7,"));
        assert_eq!(read("national_express", "shapes.txt").lines().count(), 3);
        assert!(read("national_express", "calendar_dates.txt").contains("s,20261126,1"));
        assert!(read("flixbus", "trips.txt").contains("r5,s,t5,"));
        assert!(read("berrys_coaches", "trips.txt").contains("r6,s,t6,"));
        for folder in ["alpha", "beta", "other"] {
            let trips = read(folder, "trips.txt");
            assert!(!trips.contains("t4") && !trips.contains("t5") && !trips.contains("t6"));
        }
        assert_eq!(coach_feed("OP5051", ""), Some(1));
        assert_eq!(coach_feed("OP15966", "NXHH"), None);
        fs::remove_dir_all(dir).unwrap();
    }
}
