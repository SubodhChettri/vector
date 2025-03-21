#![cfg(feature = "aws-kinesis-firehose-integration-tests")]
#![cfg(test)]

use aws_sdk_elasticsearch::Client as EsClient;
use aws_sdk_firehose::model::ElasticsearchDestinationConfiguration;
use futures::{StreamExt, TryFutureExt};
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

use super::*;
use crate::aws::aws_sdk::create_client;
use crate::config::ProxyConfig;
use crate::sinks::aws_kinesis_firehose::config::KinesisFirehoseClientBuilder;
use crate::sinks::elasticsearch::BulkConfig;
use crate::{
    aws::{AwsAuthentication, RegionOrEndpoint},
    config::{SinkConfig, SinkContext},
    sinks::{
        elasticsearch::{ElasticsearchAuth, ElasticsearchCommon, ElasticsearchConfig},
        util::{
            encoding::{EncodingConfig, StandardEncodings},
            BatchConfig, Compression, TowerRequestConfig,
        },
    },
    test_util::{
        components, components::AWS_SINK_TAGS, random_events_with_stream, random_string,
        wait_for_duration,
    },
};

fn kinesis_address() -> String {
    std::env::var("KINESIS_ADDRESS").unwrap_or_else(|_| "http://localhost:4566".into())
}

fn elasticsearch_address() -> String {
    std::env::var("ELASTICSEARCH_ADDRESS").unwrap_or_else(|_| "http://localhost:4571".into())
}

#[tokio::test]
async fn firehose_put_records() {
    let stream = gen_stream();

    let elasticseacrh_arn = ensure_elasticsearch_domain(stream.clone()).await;

    ensure_elasticesarch_delivery_stream(stream.clone(), elasticseacrh_arn.clone()).await;

    let mut batch = BatchConfig::default();
    batch.max_events = Some(2);

    let config = KinesisFirehoseSinkConfig {
        stream_name: stream.clone(),
        region: RegionOrEndpoint::with_both("localstack", kinesis_address().as_str()),
        encoding: EncodingConfig::from(StandardEncodings::Json), // required for ES destination w/ localstack
        compression: Compression::None,
        batch,
        request: TowerRequestConfig {
            timeout_secs: Some(10),
            retry_attempts: Some(0),
            ..Default::default()
        },
        tls: None,
        auth: Default::default(),
        acknowledgements: Default::default(),
    };

    let cx = SinkContext::new_test();

    let sink = config.build(cx).await.unwrap();

    let (input, events) = random_events_with_stream(100, 100, None);

    components::init_test();
    sink.0.run(events.map(Into::into)).await.unwrap();

    sleep(Duration::from_secs(5)).await;
    components::SINK_TESTS.assert(&AWS_SINK_TAGS);

    let config = ElasticsearchConfig {
        auth: Some(ElasticsearchAuth::Aws(AwsAuthentication::Default {})),
        endpoint: elasticsearch_address(),
        bulk: Some(BulkConfig {
            index: Some(stream.clone()),
            action: None,
        }),
        ..Default::default()
    };
    let common = ElasticsearchCommon::parse_config(&config)
        .await
        .expect("Config error");

    let client = reqwest::Client::builder()
        .build()
        .expect("Could not build HTTP client");

    let response = client
        .get(&format!("{}/{}/_search", common.base_url, stream))
        .json(&json!({
            "query": { "query_string": { "query": "*" } }
        }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .expect("could not issue Elasticsearch search request");

    let total = response["hits"]["total"]["value"]
        .as_u64()
        .expect("Elasticsearch response does not include hits->total->value");
    assert_eq!(input.len() as u64, total);

    let hits = response["hits"]["hits"]
        .as_array()
        .expect("Elasticsearch response does not include hits->hits");
    #[allow(clippy::needless_collect)] // https://github.com/rust-lang/rust-clippy/issues/6909
    let input = input
        .into_iter()
        .map(|rec| serde_json::to_value(&rec.into_log()).unwrap())
        .collect::<Vec<_>>();
    for hit in hits {
        let hit = hit
            .get("_source")
            .expect("Elasticsearch hit missing _source");
        assert!(input.contains(hit));
    }
}

fn test_region_endpoint() -> RegionOrEndpoint {
    RegionOrEndpoint::with_both("localstack", kinesis_address())
}

async fn firehose_client() -> aws_sdk_firehose::Client {
    let region_endpoint = test_region_endpoint();
    let auth = AwsAuthentication::test_auth();
    let proxy = ProxyConfig::default();

    create_client::<KinesisFirehoseClientBuilder>(
        &auth,
        region_endpoint.region(),
        region_endpoint.endpoint().unwrap(),
        &proxy,
        &None,
    )
    .await
    .unwrap()
}

/// creates ES domain with the given name and returns the ARN
async fn ensure_elasticsearch_domain(domain_name: String) -> String {
    let client = EsClient::from_conf(
        aws_sdk_elasticsearch::config::Builder::new()
            .credentials_provider(
                AwsAuthentication::test_auth()
                    .credentials_provider()
                    .await
                    .unwrap(),
            )
            .endpoint_resolver(test_region_endpoint().endpoint().unwrap().unwrap())
            .region(test_region_endpoint().region())
            .build(),
    );

    let arn = match client
        .create_elasticsearch_domain()
        .domain_name(domain_name)
        .send()
        .await
    {
        Ok(res) => res
            .domain_status
            .expect("no domain status")
            .arn
            .expect("arn expected"),
        Err(error) => panic!("Unable to create the Elasticsearch domain {:?}", error),
    };

    // wait for ES to be available; it starts up when the ES domain is created
    // This takes a long time
    wait_for_duration(
        || async {
            reqwest::get(format!("{}/_cluster/health", elasticsearch_address()))
                .and_then(reqwest::Response::json::<Value>)
                .await
                .map(|v| {
                    v.get("status")
                        .and_then(|status| status.as_str())
                        .map(|status| status == "green")
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        },
        Duration::from_secs(60),
    )
    .await;

    arn
}

/// creates Firehose delivery stream to ship to Elasticsearch
async fn ensure_elasticesarch_delivery_stream(
    delivery_stream_name: String,
    elasticseacrh_arn: String,
) {
    let client = firehose_client().await;

    match client
        .create_delivery_stream()
        .delivery_stream_name(delivery_stream_name.clone())
        .elasticsearch_destination_configuration(
            ElasticsearchDestinationConfiguration::builder()
                .index_name(delivery_stream_name)
                .domain_arn(elasticseacrh_arn)
                .role_arn("doesn't matter")
                .type_name("doesn't matter")
                .build(),
        )
        .send()
        .await
    {
        Ok(_) => (),
        Err(error) => panic!("Unable to create the delivery stream {:?}", error),
    };
}

fn gen_stream() -> String {
    format!("test-{}", random_string(10).to_lowercase())
}
