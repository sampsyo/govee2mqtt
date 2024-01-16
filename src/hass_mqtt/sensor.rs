use crate::commands::serve::POLL_INTERVAL;
use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::instance::{publish_entity_config, EntityInstance};
use crate::platform_api::DeviceCapability;
use crate::service::device::Device as ServiceDevice;
use crate::service::hass::{availability_topic, topic_safe_id, topic_safe_string, HassClient};
use crate::service::quirks::HumidityUnits;
use crate::service::state::StateHandle;
use crate::temperature::{ctof, TemperatureUnits};
use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use serde_json::json;

#[derive(Serialize, Clone, Debug)]
pub struct SensorConfig {
    #[serde(flatten)]
    pub base: EntityConfig,

    pub state_topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_of_measurement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_attributes_topic: Option<String>,
}

impl SensorConfig {
    pub async fn publish(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        publish_entity_config("sensor", state, client, &self.base, self).await
    }

    pub async fn notify_state(&self, client: &HassClient, value: &str) -> anyhow::Result<()> {
        client.publish(&self.state_topic, value).await
    }
}

#[derive(Clone)]
pub struct GlobalFixedDiagnostic {
    sensor: SensorConfig,
    value: String,
}

#[async_trait]
impl EntityInstance for GlobalFixedDiagnostic {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(&state, &client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.notify_state(&client, &self.value).await
    }
}

impl GlobalFixedDiagnostic {
    pub fn new<NAME: Into<String>, VALUE: Into<String>>(name: NAME, value: VALUE) -> Self {
        let name = name.into();
        let unique_id = format!("global-{}", topic_safe_string(&name));

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some(name),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::this_service(),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                unit_of_measurement: None,
                json_attributes_topic: None,
            },
            value: value.into(),
        }
    }
}

#[derive(Clone)]
pub struct CapabilitySensor {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
    instance_name: String,
}

impl CapabilitySensor {
    pub async fn new(
        device: &ServiceDevice,
        state: &StateHandle,
        instance: &DeviceCapability,
    ) -> anyhow::Result<Self> {
        let unique_id = format!(
            "sensor-{id}-{inst}",
            id = topic_safe_id(device),
            inst = topic_safe_string(&instance.instance)
        );

        let unit_of_measurement = match instance.instance.as_str() {
            "sensorTemperature" => Some("°C".to_string()),
            "sensorHumidity" => Some("%".to_string()),
            _ => None,
        };

        let name = match instance.instance.as_str() {
            "sensorTemperature" => "Temperature".to_string(),
            "sensorHumidity" => "Humidity".to_string(),
            "online" => "Connected to Govee Cloud".to_string(),
            _ => instance.instance.to_string(),
        };

        Ok(Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some(name),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                unit_of_measurement,
                json_attributes_topic: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
            instance_name: instance.instance.to_string(),
        })
    }

    pub fn into_temperature_farenheit(mut self) -> Option<Self> {
        if self.instance_name != "sensorTemperature" {
            return None;
        }

        self.sensor.unit_of_measurement.replace("°F".to_string());
        self.sensor.base.unique_id.push_str("_F");
        self.sensor.state_topic.push_str("_F");
        self.sensor
            .base
            .name
            .replace("Temperature (imperial)".to_string());
        Some(self)
    }
}

#[async_trait]
impl EntityInstance for CapabilitySensor {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(&state, &client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        let quirk = device.resolve_quirk();

        if let Some(state) = &device.http_device_state {
            for cap in &state.capabilities {
                if cap.instance == self.instance_name {
                    let value = match self.instance_name.as_str() {
                        "sensorTemperature" => {
                            let units = quirk
                                .and_then(|q| q.platform_temperature_sensor_units)
                                .unwrap_or(TemperatureUnits::Celsius);

                            match cap
                                .state
                                .pointer("/value")
                                .and_then(|v| v.as_f64())
                                .map(|v| units.from_reading_to_celsius(v))
                            {
                                Some(v) => match self.sensor.unit_of_measurement.as_deref() {
                                    Some("°F") => format!("{:.2}", ctof(v)),
                                    _ => format!("{v:.2}"),
                                },
                                None => "".to_string(),
                            }
                        }
                        "sensorHumidity" => {
                            let units = quirk
                                .and_then(|q| q.platform_humidity_sensor_units)
                                .unwrap_or(HumidityUnits::RelativePercent);
                            match cap
                                .state
                                .pointer("/value/currentHumidity")
                                .and_then(|v| v.as_f64())
                                .map(|v| units.from_reading_to_relative_percent(v))
                            {
                                Some(v) => format!("{v:.2}"),
                                None => "".to_string(),
                            }
                        }
                        _ => cap.state.to_string(),
                    };

                    return self.sensor.notify_state(&client, &value).await;
                }
            }
        }
        log::trace!(
            "CapabilitySensor::notify_state: didn't find state for {device} {instance}",
            instance = self.instance_name
        );
        Ok(())
    }
}

pub struct DeviceStatusDiagnostic {
    sensor: SensorConfig,
    device_id: String,
    state: StateHandle,
}

impl DeviceStatusDiagnostic {
    pub fn new(device: &ServiceDevice, state: &StateHandle) -> Self {
        let unique_id = format!("sensor-{id}-gv2mqtt-status", id = topic_safe_id(device),);

        Self {
            sensor: SensorConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: Some("Status".to_string()),
                    entity_category: Some("diagnostic".to_string()),
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id: unique_id.clone(),
                    device_class: None,
                    icon: None,
                },
                state_topic: format!("gv2mqtt/sensor/{unique_id}/state"),
                json_attributes_topic: Some(format!("gv2mqtt/sensor/{unique_id}/attributes")),
                unit_of_measurement: None,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        }
    }
}

#[async_trait]
impl EntityInstance for DeviceStatusDiagnostic {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        self.sensor.publish(&state, &client).await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        let iot_state = device.compute_iot_device_state();
        let lan_state = device.compute_lan_device_state();
        let http_state = device.compute_http_device_state();
        let platform_metadata = &device.http_device_info;
        let platform_state = &device.http_device_state;
        let device_state = device.device_state();

        let now = Utc::now();

        let threshold = *POLL_INTERVAL + chrono::Duration::seconds(30);

        let summary = match &device_state {
            Some(state) => {
                if now - state.updated > threshold {
                    "Missing".to_string()
                } else {
                    "Available".to_string()
                }
            }
            None => "Unknown".to_string(),
        };

        let attributes = json!({
            "iot": iot_state,
            "lan": lan_state,
            "http": http_state,
            "platform_metadata": platform_metadata,
            "platform_state": platform_state,
            "overall": device_state,
        });

        self.sensor.notify_state(&client, &summary).await?;
        if let Some(topic) = &self.sensor.json_attributes_topic {
            client.publish_obj(topic, attributes).await?;
        }
        Ok(())
    }
}
