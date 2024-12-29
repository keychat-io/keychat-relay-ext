# keychat-relay-gas: ext/src/gas.rs

Pay to nostr-rs-relay with cashu by grpc authorization server

##### Tip: rust nightly required

# keychat-relay-sas: ext/src/sas.rs

Pay to api with cashu by storage authorization server

##### Nginx conf
```ini
location ^~/api/ {
               proxy_pass  http://127.0.0.1:3001/;
               proxy_set_header Host $proxy_host;
               proxy_set_header X-Real-IP $remote_addr;
               proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
               proxy_http_version 1.1;
           }
```

##### Docker
```sh
docker run -d --name kcgas -v $PWD:/opt --network host debian:12 /opt/keychat-relay-gas -v -c /opt/gas.toml
docker logs --tail 100 kcgas -f

docker run -d --name kcsas -v $PWD:/opt -w /opt --network host debian:12 /opt/keychat-relay-sas -v -c /opt/sas.toml
docker logs --tail 100 kcsas -f
```
