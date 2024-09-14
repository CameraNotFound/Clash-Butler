use std::{env, fs};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;

use clap::Parser;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::clash::{ClashMeta, DelayTestConfig};
use crate::proxy::{parse_conf};
use crate::settings::Settings;
use crate::sub::{include_names, save_proxies_into_clash_file, SubConverter};

mod sub;
mod clash;
mod routes;
mod risk;
mod server;
mod ip;
mod cgi_trace;
mod settings;
mod speedtest;
mod proxy;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    // Starts the Axum server
    #[arg(long)]
    server: bool,

    // Just test subs/test/config.yaml
    #[arg(long)]
    test: bool,
}

const TEST_PROXY_NAME: &str = "PROXY";

#[tokio::main]
async fn main() {
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .finish()
    ).expect("setting default subscriber failed");
    let args = Cli::parse();
    let config = Settings::new();
    match config {
        Ok(mut config) => {
            // 创建订阅测试所用的目录结构
            create_folder();
            if args.server {
                // 服务端
                // server::start_server(config).await
            } else {
                // 本地生成
                if args.test {
                    config.test = Some(true);
                }
                run(config).await
            }
        }
        Err(e) => {
            panic!("配置文件读取失败: {}", e)
        }
    }
}

async fn run(config: Settings) {
    let test_yaml_path = env::current_dir().unwrap().join("subs/test/config.yaml");
    let release_yaml_path = env::current_dir().unwrap().join("subs/release/clash.yaml");
    let test_clash_template_path = "conf/clash_test.yaml";
    let release_clash_template_path = "conf/clash_release.yaml";
    let test_proxies;
    if config.test.is_some() {
        if !test_yaml_path.exists() {
            error!("当前并没有找到可用的测试文件，请删掉 --test 后重试");
            return;
        }
        test_proxies = parse_conf(&test_yaml_path).unwrap();
    } else {
        let mut urls = config.subs;
        if config.need_add_pool {
            urls.extend(config.pools)
        }
        test_proxies = SubConverter::get_proxies(&urls).await;

        if test_proxies.is_empty() {
            error!("当前无可用的待测试订阅连接，请修改配置文件添加订阅链接或确保当前网络通顺");
            return;
        }
    }

    let proxies_group: Vec<_> = test_proxies
        .chunks(200)
        .map(|p| p.to_vec())
        .collect();
    let group_size = proxies_group.len();
    if group_size > 1 {
        info!("待测试代理数量达到 {} 个，因此以 200 为限制分为 {} 组测试，加速测试速度", test_proxies.len(), proxies_group.len());
    }

    // 启动 Clash 内核
    let external_port = 9091;
    let mixed_port = 7999;
    let mut useful_proxies = Vec::new();
    for (index, proxies) in proxies_group.iter().enumerate() {
        if group_size > 1 {
            info!("正在测试第 {} 组", index + 1)
        }
        save_proxies_into_clash_file(&proxies,
                                     test_clash_template_path.to_string(),
                                     test_yaml_path.to_string_lossy().to_string());
        let mut clash_meta = ClashMeta::new(external_port, mixed_port);
        if let Err(e) = clash_meta.start().await {
            error!("原神启动失败，第一次启动可能会下载 geo 相关的文件，重新启动即可，打开 logs/clash.log，查看具体错误原因，{}", e);
            clash_meta.stop().unwrap();
            return;
        }

        match clash_meta.get_group(TEST_PROXY_NAME).await {
            Ok(nodes) => {
                info!("开始测试 subs/test/config.yaml 中节点的延迟速度，节点总数：{}", nodes.all.len())
            }
            Err(e) => {
                error!("获取节点数失败，请检查 clash 日志文件和 subs/test/config.yaml 生成的节点是否正确, {}", e);
                clash_meta.stop().unwrap();
                return;
            }
        }

        info!("开始测试连通性");
        let delay_results = test_node_with_delay_config(&clash_meta, &config.connect_test).await;
        let nodes = get_all_tested_nodes(&delay_results);
        info!("连通性测试结果：{} 个节点可用", nodes.len());
        if !nodes.is_empty() {
            useful_proxies.extend(include_names(proxies.to_vec(), nodes))
        }
        clash_meta.stop().unwrap();
    }

    if useful_proxies.is_empty() {
        error!("当前无可用节点，请尝试更换订阅节点或重试");
        return;
    }

    if config.fast_mode {
        save_proxies_into_clash_file(&useful_proxies, release_clash_template_path.to_string(), release_yaml_path.to_string_lossy().to_string());
        info!("release 文件地址：{}", release_yaml_path.to_string_lossy());
    } else {
        let mut clash_meta = ClashMeta::new(external_port, mixed_port);
        save_proxies_into_clash_file(&useful_proxies,
                                     test_clash_template_path.to_string(),
                                     test_yaml_path.to_string_lossy().to_string());

        if let Err(e) = clash_meta.start().await {
            error!("原神启动失败，第一次启动可能会下载 geo 相关的文件，重新启动即可，打开 logs/clash.log，查看具体错误原因，{}", e);
            clash_meta.stop().unwrap();
            return;
        }

        let mut nodes = vec![];
        let mut top_node = String::new();
        for (name, conf) in config.websites {
            info!("当前测试站点：{}, {}", name, conf.url);
            let delay_results = test_node_with_delay_config(&clash_meta, &conf).await;
            if !delay_results.is_empty() {
                nodes = get_all_tested_nodes(&delay_results);
                top_node = get_top_node(&delay_results);
                info!("可用节点数：{}", nodes.len());
                info!("最低延迟节点：{}", top_node);
            }
        }

        let mut node_rename_map: HashMap<String, String> = HashMap::new();
        let mut node_ip_map: HashMap<String, IpAddr> = HashMap::new();
        if config.rename_node {
            if nodes.is_empty() {
                error!("当前无可用节点，请尝试更换订阅节点或重试");
                clash_meta.stop().unwrap();
                return;
            }
            let count = config.rename_pattern.matches('_').count();
            let mut i = 0;
            while i < nodes.len() {
                let node = &nodes[i];
                // 如果当前节点名称与需要重命名的格式下划线个数一致，暂时认为就是已经格式化好的，因此跳过
                if node.matches('_').count() == count && !node.contains("github.com") {
                    info!("「{}」已符合重命名结构，跳过", node);
                    i += 1;
                    continue;
                }

                let ip_result = clash_meta.set_group_proxy(TEST_PROXY_NAME, node).await;
                if ip_result.is_ok() {
                    let ip_result = cgi_trace::get_ip(&clash_meta.proxy_url).await;
                    if ip_result.is_ok() {
                        let proxy_ip = ip_result.unwrap();
                        info!("「{}」ip: {}", node, proxy_ip);
                        node_ip_map.insert(node.clone(), proxy_ip);
                        i += 1;
                    } else {
                        let err_msg = ip_result.err().unwrap();
                        error!("获取节点 {} 的 IP 失败, {}", node, err_msg);
                        nodes.remove(i);
                    }
                } else {
                    let err_msg = ip_result.err().unwrap();
                    error!("设置节点 {} 失败, {}", node, err_msg);
                    i += 1;
                }
            }

            if clash_meta.set_group_proxy(TEST_PROXY_NAME, &top_node).await.is_ok() {
                for (node, ip) in &node_ip_map {
                    let ip_detail_result = ip::get_ip_detail(ip, &clash_meta.proxy_url).await;
                    match ip_detail_result {
                        Ok(ip_detail) => {
                            info!("{:?}", ip_detail);
                            if config.rename_node {
                                let new_name = config.rename_pattern
                                    .replace("${IP}", &ip.to_string())
                                    .replace("${COUNTRYCODE}", &ip_detail.country_code)
                                    .replace("${ISP}", &ip_detail.isp)
                                    .replace("${CITY}", &ip_detail.city);
                                node_rename_map.insert(node.clone(), new_name);
                            }
                        }
                        Err(e) => {
                            error!("获取节点 {node} 的 IP 信息失败, {e}");
                        }
                    }
                }
            };
        }

        let mut release_proxies = include_names(useful_proxies, nodes);
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        if !node_rename_map.is_empty() {
            for proxy in &mut release_proxies {
                let mut name = if let Some(new_name) = node_rename_map.get(proxy.get_name()) {
                    new_name.clone()
                } else {
                    node_ip_map.get(proxy.get_name()).unwrap().clone().to_string()
                };
                let count = name_counts.entry(name.clone()).or_insert(0);
                if *count > 0 {
                    name = format!("{}{}", name, count);
                }
                proxy.set_name(&name);
                *count += 1;
            }
        }

        save_proxies_into_clash_file(&release_proxies, release_clash_template_path.to_string(), release_yaml_path.to_string_lossy().to_string());
        info!("release 文件地址：{}", release_yaml_path.to_string_lossy());
        clash_meta.stop().unwrap();
    }
}

fn get_top_node(test_results: &Vec<HashMap<String, i64>>) -> String {
    let mut combined_data: HashMap<String, Vec<i64>> = HashMap::new();
    for test in test_results {
        for (node, latency) in test {
            combined_data.entry(node.clone()).or_default().push(*latency);
        }
    }
    let node_stats: Vec<(String, i64)> = combined_data.clone()
        .into_iter()
        .map(|(node, latencies)| {
            let sum: i64 = latencies.iter().sum();
            let count = latencies.len() as i64;
            let mean = sum / count;
            (node, mean)
        })
        .collect();
    node_stats.into_iter().min_by_key(|(_, mean)| *mean).unwrap().0
}

async fn test_node_with_delay_config(clash_meta: &ClashMeta, delay_test_config: &DelayTestConfig) -> Vec<HashMap<String, i64>> {
    const ROUND: i32 = 10;
    info!("测试配置：{:?}", delay_test_config);
    let mut delay_results = vec![];

    // 预热 2 轮，DNS lookup
    for _ in 0..2 {
        let _ = clash_meta.test_group(TEST_PROXY_NAME, delay_test_config).await;
    }

    for n in 0..ROUND {
        info!("测试第 {} 轮", n + 1);
        let result = clash_meta.test_group(TEST_PROXY_NAME, delay_test_config).await;

        match result {
            Ok(delay) => {
                delay_results.push(delay.clone());
                info!("有速度节点个数为：{}", delay.len())
            }
            Err(e) => {
                info!("当前测试轮完全没有速度, {}", e)
            }
        }
    }
    delay_results
}

/*
获取所有已测速有过一次速度的节点
 */
fn get_all_tested_nodes(test_results: &Vec<HashMap<String, i64>>) -> Vec<String> {
    let mut keys_set = HashSet::new();
    for result in test_results {
        for key in result.keys() {
            keys_set.insert(key.clone());
        }
    }
    keys_set.into_iter().collect()
}

/*
获取测速稳定的节点
 */
#[allow(dead_code)]
fn get_stable_tested_nodes(test_results: &Vec<HashMap<String, i64>>) -> Vec<String> {
    // 合并所有测试数据
    let mut combined_data: HashMap<String, Vec<i64>> = HashMap::new();
    for test in test_results {
        for (node, latency) in test {
            combined_data.entry(node.clone()).or_default().push(*latency);
        }
    }

    // 计算每个节点的平均延迟和标准差
    let mut node_stats: Vec<(String, f64)> = combined_data.clone()
        .into_iter()
        .filter_map(|(node, latencies)| {
            let sum: i64 = latencies.iter().sum();
            let count = latencies.len();
            if count <= combined_data.len() / 2 {
                None
            } else {
                let mean = sum as f64 / count as f64;
                Some((node, mean))
            }
        })
        .collect();

    // 根据平均延迟对稳定的节点进行排序
    node_stats.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    node_stats.into_iter().map(|(node, _)| node).collect()
}

// 创建目录
fn create_folder() {
    let logs_path = "logs";
    if !Path::new(logs_path).exists() {
        fs::create_dir(logs_path).unwrap()
    }

    let subs_path = "subs";
    if !Path::new(subs_path).exists() {
        fs::create_dir(subs_path).unwrap();
    }

    let test_path = "subs/test";
    if !Path::new(test_path).exists() {
        fs::create_dir(test_path).unwrap();
    }

    let release_path = "subs/release";
    if !Path::new(release_path).exists() {
        fs::create_dir(release_path).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_stable_nodes() {
        // [
        //     { "免费节点2": 829 },
        //     { "免费节点3": 815, "免费节点2": 945, "免费节点1": 838 },
        //     { "免费节点4": 835, "免费节点1": 850, "免费节点3": 819 },
        //     { "免费节点1": 844, "免费节点3": 830, "免费节点2": 856 },
        //     { "免费节点3": 857, "免费节点4": 796, "2": 911, "免费节点4": 816 },
        //     { "免费节点1": 895, "免费节点3": 863, "免费节点4": 829 },
        //     { "免费节点3": 837, "免费节点1": 809, "免费节点4": 849 },
        //     { "免费节点3": 849, "免费节点2": 904, "免费节点4": 892 }
        // ];

        // 假设这是从十组测试中收集的数据
        let test_data = vec![
            HashMap::from([("node1".to_string(), 100), ("node2".to_string(), 200), ("node3".to_string(), 150)]),
            HashMap::from([("node1".to_string(), 110), ("node2".to_string(), 190), ("node3".to_string(), 160)]),
            HashMap::from([("node1".to_string(), 120), ("node3".to_string(), 10000)]),
        ];

        println!("{:?}", get_top_node(&test_data));
    }

    #[test]
    fn test_rename_pattern() {
        let count = "${COUNTRYCODE}_${CITY}_${ISP}".matches('_').count();
        println!("{count}");
        let count = "HongKong_Jordan_VertexConnectivityLLC62".matches('_').count();
        println!("{count}")
    }
}