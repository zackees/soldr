crate::timed_test!(
    mixed_workload_never_exceeds_bulk_plus_quick_total_capacity,
    Duration::from_secs(15),
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
        std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "3");

        runtime().block_on(async {
                // One large (segmentable) download against Bulk, plus
                // several small control-style requests against Quick, all
                // concurrent, hitting servers that each track their own
                // concurrency high-water-mark. The combined observed
                // concurrency on each side must never exceed that pool's
                // own size.
                let bulk = SocketPool::new(2);
                let quick = SocketPool::new(2);

                let big_body: Vec<u8> = (0..3072u32).map(|i| (i % 241) as u8).collect();
                let (big_url, big_hw) = serve_range_tracking_concurrency(big_body.clone()).await;
                let big_resp = reqwest::Client::new()
                    .get(&big_url)
                    .send()
                    .await
                    .expect("GET big");
                let big_fut = stream_response_to_temp_file_with_pool(
                    big_resp,
                    &big_url,
                    Duration::from_secs(5),
                    ASSET_SAFETY_TIMEOUT,
                    Arc::clone(&bulk),
                );

                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind small");
                let address = listener.local_addr().expect("addr small");
                let small_hw = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let small_current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let hw_task = Arc::clone(&small_hw);
                let cur_task = Arc::clone(&small_current);
                tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let hw = Arc::clone(&hw_task);
                        let cur = Arc::clone(&cur_task);
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 1024];
                            let _ = socket.read(&mut buf).await;
                            let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                            hw.fetch_max(now, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            let _ = socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                                .await;
                            let _ = socket.shutdown().await;
                            cur.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                });
                let small_url = format!("http://{address}/manifest.json");

                let small_handles: Vec<_> = (0..3)
                    .map(|_| {
                        let quick = Arc::clone(&quick);
                        let url = small_url.clone();
                        tokio::spawn(async move {
                            let client = reqwest::Client::new();
                            send_control_request_with_pool(
                                client.get(&url),
                                &url,
                                Duration::from_secs(5),
                                quick,
                            )
                            .await
                        })
                    })
                    .collect();

                let big_result = big_fut.await;
                big_result.expect("big download must complete");
                for handle in small_handles {
                    handle
                        .await
                        .expect("small task must not panic")
                        .expect("small control request must complete");
                }

                assert!(
                    big_hw.load(Ordering::SeqCst) <= 2,
                    "bulk-side concurrency must stay <= bulk pool size"
                );
                assert!(
                    small_hw.load(Ordering::SeqCst) <= 2,
                    "quick-side concurrency must stay <= quick pool size"
                );
            });

        clear_segmented_env();
    }
);
