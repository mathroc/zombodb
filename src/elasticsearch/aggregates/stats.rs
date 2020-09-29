use crate::elasticsearch::Elasticsearch;
use crate::zdbquery::ZDBQuery;
use pgx::*;
use serde::*;
use serde_json::*;

#[pg_extern(immutable, parallel_safe)]
fn stats(
    index: PgRelation,
    field: &str,
    query: ZDBQuery,
) -> impl std::iter::Iterator<
    Item = (
        name!(count, i64),
        name!(min, Numeric),
        name!(max, Numeric),
        name!(avg, Numeric),
        name!(sum, Numeric),
    ),
> {
    #[derive(Deserialize, Serialize)]
    struct StatsAggData {
        count: i64,
        min: Numeric,
        max: Numeric,
        avg: Numeric,
        sum: Numeric,
    }

    let elasticsearch = Elasticsearch::new(&index);

    let request = elasticsearch.aggregate::<StatsAggData>(
        query.prepare(&index),
        json! {
            {
                "stats": {
                    "field" : field
                }
            }
        },
    );

    let result = request
        .execute()
        .expect("failed to execute aggregate search");

    vec![(result.count, result.min, result.max, result.avg, result.sum)].into_iter()
}