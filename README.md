cargo watch -c -q -w web-server -w protocol  -x "run --bin web-server"
cargo watch -c -q -w client -w protocol  -x "run --bin client"
Fulcrum.exe ./fulcrum.conf

bitcoin-qt.exe  -regtest -txindex=1 -rpcuser=abc -rpcpassword=abc -bind=127.0.0.1:18445 -server=1

monero-linux-x64-v0.18.3.1.tar.bz2

./monero-wallet-rpc --stagenet --disable-rpc-login --log-level=1 --daemon-address=http://stagenet.xmr-tw.org:38081 --untrusted-daemon --confirm-external-bind --rpc-bind-ip=0.0.0.0 --rpc-bind-port=8081 --wallet-dir=wallet_dir


monerod --regtest --offline --fixed-difficulty=1 --rpc-bind-ip=0.0.0.0 --confirm-external-bind 

--allow-mismatched-daemon-version