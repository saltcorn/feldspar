//! Public datasets (`sc_api::public_datasets`): getting one makes its tables,
//! its rows and a dataset, through the row layer, in one go or not at all.
//!
//! The files are served by a fake [`Fetch`] from small **synthetic** stand-ins
//! written in each source's own format — the same header, the same `NULL` and
//! `NA` markers, a manager listed after the people who report to her — so the
//! catalogue's real entries are what is installed, without the network and
//! without copying anyone's data into this repository.
//!
//! `every_public_dataset_installs_from_its_source` is the real thing: it
//! downloads every file of every entry and installs it into a PostGIS database.
//! It is `#[ignore]`d (it needs the network and takes minutes); run it with
//! `cargo test -p sc-api --test it -- --ignored every_public_dataset`, and set
//! `FELDSPAR_PUBLIC_DATA_CACHE` to a directory to download each file once, and
//! `FELDSPAR_PUBLIC_DATASETS` to a comma-separated list of keys to install only those.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: it drives its own servers, tasks and channels; the replacements are for production paths"
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_api::public_datasets::{self, Fetch, Progress, ProgressStage};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_test_harness::TestDb;
use serde_json::{Value, json};

const NW: &str = "https://raw.githubusercontent.com/graphql-compose/graphql-compose-examples/8677f359f35a70d4a52c42b07cc30931ba78208f/examples/northwind/data/csv/";
const RD: &str = "https://raw.githubusercontent.com/vincentarelbundock/Rdatasets/1dcc2bf5f955cc1224a3e1307256e1fe86b68dae/csv/";
const NUTS: &str = "https://gisco-services.ec.europa.eu/distribution/v2/nuts/geojson/NUTS_RG_60M_2021_4326.geojson";
const GAPMINDER: &str = "https://raw.githubusercontent.com/jennybc/gapminder/391b5a40d3574c3b89d889dd7d72f7f3fdb4a70c/inst/extdata/gapminder.tsv";

/// Files by URL; a URL it does not have is a failed download.
struct Fixtures(HashMap<String, String>);

#[async_trait]
impl Fetch for Fixtures {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        self.0
            .get(url)
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| Error::msg(format!("404 for {url}")))
    }
}

fn fixtures(files: &[(String, &str)]) -> Fixtures {
    Fixtures(
        files
            .iter()
            .map(|(url, body)| (url.clone(), body.to_string()))
            .collect(),
    )
}

/// A synthetic Northwind: every table, in the export's format.
fn northwind() -> Fixtures {
    let nw = |file: &str| format!("{NW}{file}");
    fixtures(&[
        (
            nw("categories.csv"),
            "categoryID,categoryName,description,picture\n1,Beverages,Things to drink,0x15\n2,Produce,Fruit and vegetables,0x16\n",
        ),
        (
            nw("suppliers.csv"),
            "supplierID,companyName,contactName,contactTitle,address,city,region,postalCode,country,phone,fax,homePage\n1,Acme Drinks,Ann Smith,Manager,1 High St,London,NULL,EC1 4SD,UK,(171) 555-2222,NULL,NULL\n",
        ),
        (
            nw("shippers.csv"),
            "shipperID,companyName,phone\n1,Fast Ships,(503) 555-9831\n2,Slow Boats,(503) 555-3199\n",
        ),
        (
            nw("customers.csv"),
            "customerID,companyName,contactName,contactTitle,address,city,region,postalCode,country,phone,fax\nALPHA,Alpha Foods,Bo Berg,Owner,\"Main St, 5\",Berlin,NULL,12209,Germany,030-0074321,NULL\nBETA,Beta Market,Cy Dee,Buyer,2 Side St,Lyon,NULL,69004,France,78.32.54.86,NULL\n",
        ),
        (nw("regions.csv"), "regionID,regionDescription\n1,Eastern\n"),
        (
            nw("territories.csv"),
            "territoryID,territoryDescription,regionID\n01581,Westboro,1\n01730,Bedford,1\n",
        ),
        // The vice president is listed after the people who report to her.
        (
            nw("employees.csv"),
            "employeeID,lastName,firstName,title,titleOfCourtesy,birthDate,hireDate,address,city,region,postalCode,country,homePhone,extension,photo,notes,reportsTo,photoPath\n1,Able,Ann,Sales Representative,Ms.,1948-12-08 00:00:00.000,1992-05-01 00:00:00.000,1 A St,Seattle,WA,98122,USA,(206) 555-9857,5467,0x15,\"Ann likes tea, and cake.\",3,http://x\n2,Baker,Ben,Sales Representative,Mr.,1963-08-30 00:00:00.000,1992-04-01 00:00:00.000,2 B St,Kirkland,WA,98033,USA,(206) 555-3412,3355,0x15,Ben.,1,http://x\n3,Chief,Cleo,Vice President Sales,Dr.,1952-02-19 00:00:00.000,1992-08-14 00:00:00.000,3 C St,Tacoma,WA,98401,USA,(206) 555-9482,3457,0x15,Cleo.,NULL,http://x\n",
        ),
        (
            nw("employee_territories.csv"),
            "employeeID,territoryID\n1,01581\n2,01730\n",
        ),
        (
            nw("products.csv"),
            "productID,productName,supplierID,categoryID,quantityPerUnit,unitPrice,unitsInStock,unitsOnOrder,reorderLevel,discontinued\n1,Tea,1,1,10 boxes,18.00,39,0,10,0\n2,Kale,1,2,1 kg,4.50,0,10,5,1\n",
        ),
        (
            nw("orders.csv"),
            "orderID,customerID,employeeID,orderDate,requiredDate,shippedDate,shipVia,freight,shipName,shipAddress,shipCity,shipRegion,shipPostalCode,shipCountry\n10248,ALPHA,1,1996-07-04 00:00:00.000,1996-08-01 00:00:00.000,1996-07-16 00:00:00.000,2,32.38,Alpha Foods,\"Main St, 5\",Berlin,NULL,12209,Germany\n10249,BETA,2,1996-07-05 00:00:00.000,1996-08-16 00:00:00.000,NULL,1,11.61,Beta Market,2 Side St,Lyon,NULL,69004,France\n",
        ),
        (
            nw("order_details.csv"),
            "orderID,productID,unitPrice,quantity,discount\n10248,1,14.00,12,0\n10248,2,9.80,10,0.05\n10249,1,18.00,5,0\n",
        ),
    ])
}

/// A synthetic nycflights13: one flight's plane and one's destination are not
/// in the planes and airports tables, as in the real data.
fn nycflights() -> Fixtures {
    let rd = |file: &str| format!("{RD}nycflights13/{file}");
    fixtures(&[
        (
            rd("airlines.csv"),
            "rownames,carrier,name\n1,UA,United Air Lines Inc.\n2,AA,American Airlines Inc.\n",
        ),
        (
            rd("airports.csv"),
            "rownames,faa,name,lat,lon,alt,tz,dst,tzone\n1,EWR,Newark Liberty Intl,40.6925,-74.168667,18,-5,A,America/New_York\n2,IAH,George Bush Intercontinental,29.984433,-95.341442,97,-6,A,America/Chicago\n",
        ),
        (
            rd("planes.csv"),
            "rownames,tailnum,year,type,manufacturer,model,engines,seats,speed,engine\n1,N14228,1999,Fixed wing multi engine,BOEING,737-824,2,149,,Turbo-fan\n",
        ),
        (
            rd("weather.csv"),
            "rownames,origin,year,month,day,hour,temp,dewp,humid,wind_dir,wind_speed,wind_gust,precip,pressure,visib,time_hour\n1,EWR,2013,1,1,1,39.02,26.06,59.37,270,10.35702,,0,1012,10,2013-01-01T06:00:00Z\n",
        ),
        (
            rd("flights.csv"),
            "rownames,year,month,day,dep_time,sched_dep_time,dep_delay,arr_time,sched_arr_time,arr_delay,carrier,flight,tailnum,origin,dest,air_time,distance,hour,minute,time_hour\n1,2013,1,1,517,515,2,830,819,11,UA,1545,N14228,EWR,IAH,227,1400,5,15,2013-01-01T10:00:00Z\n2,2013,1,1,533,529,4,850,830,20,UA,1714,N99999,EWR,IAH,227,1416,5,29,2013-01-01T10:00:00Z\n3,2013,1,2,,600,,,901,,AA,1141,,EWR,SJU,,1576,6,0,2013-01-02T11:00:00Z\n",
        ),
    ])
}

/// A synthetic piece of the NUTS hierarchy, children before their parents.
fn nuts() -> Fixtures {
    let square = |x: f64| json!([[[x, 50.0], [x + 1.0, 50.0], [x + 1.0, 51.0], [x, 50.0]]]);
    let feature = |id: &str, level: u8, polygon: Value, multi: bool| {
        json!({
            "type": "Feature",
            "properties": { "NUTS_ID": id, "LEVL_CODE": level, "CNTR_CODE": "XX",
                            "NAME_LATN": format!("Region {id}"), "NUTS_NAME": id,
                            "MOUNT_TYPE": 4, "URBN_TYPE": 3, "COAST_TYPE": 3 },
            "geometry": if multi {
                json!({ "type": "MultiPolygon", "coordinates": [polygon] })
            } else {
                json!({ "type": "Polygon", "coordinates": polygon })
            }
        })
    };
    let collection = json!({
        "type": "FeatureCollection",
        "features": [
            feature("XX149", 3, square(9.0), false),
            feature("XX14", 2, square(9.0), true),
            feature("XX1", 1, square(9.0), false),
            feature("XX", 0, square(9.0), true),
        ]
    });
    Fixtures(HashMap::from([(NUTS.to_owned(), collection.to_string())]))
}

async fn setup(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_spatial(&catalog).await?;
    sc_dataset::bootstrap_datasets(&catalog).await?;
    Ok(catalog)
}

/// A progress callback that keeps what it is told.
fn recorder() -> (Arc<Mutex<Vec<Progress>>>, impl Fn(Progress) + Send + Sync) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    (seen, move |p: Progress| sink.lock().unwrap().push(p))
}

async fn rows(db: &TestDb, sql: &str) -> Vec<tokio_postgres::Row> {
    db.client().await.unwrap().query(sql, &[]).await.unwrap()
}

#[tokio::test]
async fn northwind_is_installed_with_its_references_and_a_dataset() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (seen, progress) = recorder();

    let installed = public_datasets::install(
        &catalog,
        "northwind",
        &northwind(),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    assert_eq!(installed.tables.len(), 11);
    assert_eq!(installed.dataset_name, "Northwind Traders");
    assert_eq!(installed.rows, 2 + 1 + 2 + 2 + 1 + 2 + 3 + 2 + 2 + 2 + 3);

    // The tree: each employee points at their manager, who was written first.
    let managers = rows(
        &db,
        "SELECT employee_id, reports_to FROM northwind_employees ORDER BY employee_id",
    )
    .await;
    let pairs: Vec<(i64, Option<i64>)> = managers.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(pairs, vec![(1, Some(3)), (2, Some(1)), (3, None)]);

    // Typed, with the export's NULLs as none and a comma inside quotes kept.
    let order = rows(
        &db,
        "SELECT customer_id, order_date::text, shipped_date, ship_region, ship_address \
         FROM northwind_orders WHERE order_id = 10249",
    )
    .await;
    assert_eq!(order[0].get::<_, String>(0), "BETA");
    assert_eq!(order[0].get::<_, String>(1), "1996-07-05");
    assert!(order[0].get::<_, Option<chrono::NaiveDate>>(2).is_none());
    assert!(order[0].get::<_, Option<String>>(3).is_none());
    let address = rows(
        &db,
        "SELECT ship_address FROM northwind_orders WHERE order_id = 10248",
    )
    .await;
    assert_eq!(address[0].get::<_, String>(0), "Main St, 5");
    let discontinued = rows(
        &db,
        "SELECT discontinued FROM northwind_products ORDER BY product_id",
    )
    .await;
    assert_eq!(
        discontinued.iter().map(|r| r.get(0)).collect::<Vec<bool>>(),
        vec![false, true]
    );
    // A leading zero in a text key survives.
    let territory = rows(
        &db,
        "SELECT territory_id FROM northwind_employee_territories ORDER BY id",
    )
    .await;
    assert_eq!(territory[0].get::<_, String>(0), "01581");

    // The references are the catalog's, with a name to show them by.
    let details = catalog.require("northwind_order_details")?;
    let product = details.field("product_id").expect("product_id");
    match &product.kind {
        sc_catalog::DataFieldKind::Key {
            target_table,
            summary_field,
            ..
        } => {
            assert_eq!(target_table, "northwind_products");
            assert_eq!(summary_field.as_ref().map(|f| f.0.as_str()), Some("name"));
        }
        other => panic!("product_id is not a key: {other:?}"),
    }
    // A reference to a row that is not there is refused by the database.
    let refused = db
        .client()
        .await?
        .execute(
            "INSERT INTO northwind_order_details (order_id, product_id, unit_price, quantity, discount) \
             VALUES (99999, 1, 1, 1, 0)",
            &[],
        )
        .await;
    assert!(refused.is_err(), "the foreign key is enforced");

    // The supplied keys' sequence is past the largest, so a new row numbers itself.
    let next = rows(
        &db,
        "INSERT INTO northwind_orders (customer_id) VALUES ('ALPHA') RETURNING order_id",
    )
    .await;
    assert!(next[0].get::<_, i64>(0) > 10249);

    // The dataset, with where the data is from and its licence.
    let library = sc_dataset::load_library(&catalog).await?;
    let def = library.get(installed.dataset_id).expect("the dataset");
    assert_eq!(def.base, sc_dataset::Base::table("northwind_order_details"));
    assert!(
        def.description.contains("Licence: MIT"),
        "{}",
        def.description
    );
    assert!(
        catalog
            .require("northwind_orders")?
            .description
            .contains("graphql-compose-examples"),
        "the table says where it is from"
    );

    // It said what it was doing as it went.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].stage, ProgressStage::Downloading);
    assert!(seen.iter().any(|p| p.stage == ProgressStage::Importing
        && p.subject == "northwind_order_details"
        && p.done == 3));
    assert_eq!(seen.last().map(|p| p.stage), Some(ProgressStage::Finishing));

    // Listed as got, with its dataset; getting it again downloads nothing and
    // answers the same dataset.
    let listing = public_datasets::list(&catalog).await?;
    let entry = listing.iter().find(|l| l.key == "northwind").unwrap();
    assert!(entry.installed);
    assert_eq!(entry.dataset_id, Some(installed.dataset_id.to_string()));
    let again = public_datasets::install(
        &catalog,
        "northwind",
        &Fixtures(HashMap::new()),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    assert_eq!(again.dataset_id, installed.dataset_id);
    assert_eq!(again.rows, 0);
    Ok(())
}

#[tokio::test]
async fn a_missing_plane_or_airport_is_no_reference_and_a_point_is_kept_with_postgis() -> Result<()>
{
    let (db, spatial) = match TestDb::with_postgis().await? {
        Some(db) => (db, true),
        None => (TestDb::new().await?, false),
    };
    let catalog = setup(&db).await?;
    let (_, progress) = recorder();
    public_datasets::install(
        &catalog,
        "nycflights13",
        &nycflights(),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await?;

    let flights = rows(
        &db,
        "SELECT date::text, tailnum, plane, dest, destination, dep_time, time_hour::text \
         FROM nycflights_flights ORDER BY id",
    )
    .await;
    assert_eq!(flights.len(), 3);
    assert_eq!(flights[0].get::<_, String>(0), "2013-01-01");
    assert_eq!(
        flights[0].get::<_, Option<String>>(2).as_deref(),
        Some("N14228")
    );
    // N99999 is not among the planes: the tail number is kept, the reference is none.
    assert_eq!(
        flights[1].get::<_, Option<String>>(1).as_deref(),
        Some("N99999")
    );
    assert!(flights[1].get::<_, Option<String>>(2).is_none());
    // SJU is not among the airports; a cancelled flight has no departure time.
    assert_eq!(flights[2].get::<_, String>(3), "SJU");
    assert!(flights[2].get::<_, Option<String>>(4).is_none());
    assert!(flights[2].get::<_, Option<i64>>(5).is_none());
    assert_eq!(flights[2].get::<_, String>(6), "2013-01-02 11:00:00+00");

    // The airports' point is a bonus: there with PostGIS, left out without.
    let airports = catalog.require("nycflights_airports")?;
    assert_eq!(airports.field("location").is_some(), spatial);
    if spatial {
        let point = rows(
            &db,
            "SELECT ST_X(location), ST_Y(location) FROM nycflights_airports WHERE faa = 'IAH'",
        )
        .await;
        assert!((point[0].get::<_, f64>(0) - -95.341442).abs() < 1e-9);
    }
    Ok(())
}

#[tokio::test]
async fn a_row_that_will_not_go_in_leaves_no_table_and_says_where_it_was() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (_, progress) = recorder();
    let bad = fixtures(&[(
        GAPMINDER.to_owned(),
        "country\tcontinent\tyear\tlifeExp\tpop\tgdpPercap\nA\tAsia\t1952\t28.8\t8425333\t779.4\nA\tAsia\t1957\t30.3\tlots\t820.8\n",
    )]);
    let e = public_datasets::install(
        &catalog,
        "gapminder",
        &bad,
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("`gapminder`")
            && e.contains("row 2")
            && e.contains("`pop`")
            && e.contains("lots"),
        "{e}"
    );
    assert!(catalog.get("gapminder_countries")?.is_none());
    assert!(catalog.get("gapminder")?.is_none());
    assert!(
        sc_dataset::load_library(&catalog)
            .await?
            .defs()
            .next()
            .is_none()
    );

    // A download that fails creates nothing either.
    let e = public_datasets::install(
        &catalog,
        "gapminder",
        &Fixtures(HashMap::new()),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("could not be downloaded"), "{e}");
    assert!(catalog.get("gapminder_countries")?.is_none());

    // The lookup table is cut out of the flat file once per country.
    let good = fixtures(&[(
        GAPMINDER.to_owned(),
        "country\tcontinent\tyear\tlifeExp\tpop\tgdpPercap\nA\tAsia\t1952\t28.8\t8425333\t779.4\nA\tAsia\t1957\t30.3\t9240934\t820.8\nB\tEurope\t1952\t55.2\t1282697\t1601.1\n",
    )]);
    public_datasets::install(
        &catalog,
        "gapminder",
        &good,
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    let countries = rows(
        &db,
        "SELECT country, continent FROM gapminder_countries ORDER BY country",
    )
    .await;
    assert_eq!(countries.len(), 2);

    // A table of one of its names that is not its own is in the way.
    db.client()
        .await?
        .batch_execute("CREATE TABLE radon_counties (id int primary key)")
        .await
        .map_err(|e| Error::database(e.to_string()))?;
    catalog.reload().await?;
    let listing = public_datasets::list(&catalog).await?;
    let radon = listing.iter().find(|l| l.key == "radon").unwrap();
    assert!(!radon.installed);
    assert!(
        radon
            .unavailable
            .as_deref()
            .unwrap_or_default()
            .contains("radon_counties")
    );
    Ok(())
}

#[tokio::test]
async fn a_spatial_dataset_needs_postgis() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = setup(&db).await?;
    let (_, progress) = recorder();
    let listing = public_datasets::list(&catalog).await?;
    let nuts_entry = listing.iter().find(|l| l.key == "eu_nuts_regions").unwrap();
    assert!(
        nuts_entry
            .unavailable
            .as_deref()
            .unwrap_or_default()
            .contains("PostGIS"),
        "{:?}",
        nuts_entry.unavailable
    );
    // A tabular one is offered.
    assert!(
        listing
            .iter()
            .find(|l| l.key == "iris")
            .unwrap()
            .unavailable
            .is_none()
    );
    let e = public_datasets::install(
        &catalog,
        "eu_nuts_regions",
        &nuts(),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("PostGIS"), "{e}");
    assert!(catalog.get("eu_nuts_regions")?.is_none());
    Ok(())
}

#[tokio::test]
async fn the_nuts_hierarchy_is_written_parents_first_with_multipolygons() -> Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let catalog = setup(&db).await?;
    let (_, progress) = recorder();
    public_datasets::install(
        &catalog,
        "eu_nuts_regions",
        &nuts(),
        &progress,
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    let regions = rows(
        &db,
        "SELECT nuts_id, parent, level, GeometryType(geom) FROM eu_nuts_regions ORDER BY nuts_id",
    )
    .await;
    let found: Vec<(String, Option<String>, i64, String)> = regions
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    assert_eq!(
        found,
        vec![
            ("XX".into(), None, 0, "MULTIPOLYGON".into()),
            ("XX1".into(), Some("XX".into()), 1, "MULTIPOLYGON".into()),
            ("XX14".into(), Some("XX1".into()), 2, "MULTIPOLYGON".into()),
            (
                "XX149".into(),
                Some("XX14".into()),
                3,
                "MULTIPOLYGON".into()
            ),
        ]
    );
    Ok(())
}

// --- the real downloads ------------------------------------------------------------

/// Downloads over HTTPS, keeping each file in `FELDSPAR_PUBLIC_DATA_CACHE`
/// when that is set.
struct Http {
    client: reqwest::Client,
    cache: Option<std::path::PathBuf>,
}

#[async_trait]
impl Fetch for Http {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let cached = self.cache.as_ref().map(|dir| {
            dir.join(
                url.chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '.' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>(),
            )
        });
        if let Some(path) = &cached
            && let Ok(bytes) = std::fs::read(path)
        {
            return Ok(bytes);
        }
        let response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| Error::msg(e.to_string()))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::msg(e.to_string()))?
            .to_vec();
        if let Some(path) = &cached {
            let _ = std::fs::write(path, &bytes);
        }
        Ok(bytes)
    }
}

#[tokio::test]
#[ignore = "downloads about 85 MB from the datasets' publishers"]
async fn every_public_dataset_installs_from_its_source() -> Result<()> {
    let Some(db) = TestDb::with_postgis().await? else {
        return Ok(());
    };
    let catalog = setup(&db).await?;
    let fetch = Http {
        client: reqwest::Client::new(),
        cache: std::env::var_os("FELDSPAR_PUBLIC_DATA_CACHE").map(Into::into),
    };
    let progress = |_: Progress| {};
    // `FELDSPAR_PUBLIC_DATASETS=iris,radon` installs only those.
    let only: Option<Vec<String>> = std::env::var("FELDSPAR_PUBLIC_DATASETS")
        .ok()
        .map(|keys| keys.split(',').map(|k| k.trim().to_owned()).collect());
    let mut failures = Vec::new();
    for entry in public_datasets::catalogue()? {
        if only.as_ref().is_some_and(|only| !only.contains(&entry.key)) {
            continue;
        }
        let started = std::time::Instant::now();
        match public_datasets::install(
            &catalog,
            &entry.key,
            &fetch,
            &progress,
            sc_catalog::Unchecked::system("test"),
        )
        .await
        {
            Ok(done) => {
                let elapsed = started.elapsed();
                eprintln!(
                    "{:<24} {:>8} rows in {:>6.1}s ({:.0} rows/s)",
                    entry.key,
                    done.rows,
                    elapsed.as_secs_f64(),
                    done.rows as f64 / elapsed.as_secs_f64().max(0.001)
                );
                // Within a tenth of the catalogue's estimate (the live feeds
                // move; the earthquakes most of all).
                let estimate = entry.rows() as f64;
                if entry.key != "earthquakes"
                    && ((done.rows as f64) - estimate).abs() > estimate * 0.1
                {
                    failures.push(format!(
                        "{}: {} rows, expected about {estimate}",
                        entry.key, done.rows
                    ));
                }
            }
            Err(e) => failures.push(format!("{}: {}", entry.key, e.causes())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}
