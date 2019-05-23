use clarity::abi::encode_call;
use clarity::{Address, PrivateKey};
use failure::bail;
use failure::Error;
use futures::Future;
use futures_timer::FutureExt;
use num::Bounded;
use num256::Uint256;
use std::time::Duration;
use web30::client::Web3;
use web30::types::SendTxOption;

#[derive(Clone)]
pub struct TokenBridge {
    xdai_web3: Web3,
    eth_web3: Web3,
    uniswap_address: Address,
    /// This is the address of the xDai bridge on Eth
    xdai_foreign_bridge_address: Address,
    /// This is the address of the xDai bridge on xDai
    xdai_home_bridge_address: Address,
    /// This is the address of the Dai token contract on Eth
    foreign_dai_contract_address: Address,
    own_address: Address,
    secret: PrivateKey,
}

impl TokenBridge {
    pub fn new(
        uniswap_address: Address,
        xdai_home_bridge_address: Address,
        xdai_foreign_bridge_address: Address,
        foreign_dai_contract_address: Address,
        own_address: Address,
        secret: PrivateKey,
        eth_full_node_url: String,
        xdai_full_node_url: String,
    ) -> TokenBridge {
        TokenBridge {
            uniswap_address,
            xdai_home_bridge_address,
            xdai_foreign_bridge_address,
            foreign_dai_contract_address,
            own_address,
            secret,
            xdai_web3: Web3::new(&xdai_full_node_url, Duration::from_secs(10)),
            eth_web3: Web3::new(&eth_full_node_url, Duration::from_secs(10)),
        }
    }

    /// Price of ETH in Dai
    pub fn eth_to_dai_price(&self, amount: Uint256) -> Box<Future<Item = Uint256, Error = Error>> {
        let web3 = self.eth_web3.clone();
        let uniswap_address = self.uniswap_address.clone();
        let own_address = self.own_address.clone();

        Box::new(
            web3.contract_call(
                uniswap_address,
                "getEthToTokenInputPrice(uint256)",
                &[amount.into()],
                own_address,
            )
            .and_then(move |tokens_bought| {
                Ok(Uint256::from_bytes_be(match tokens_bought.get(0..32) {
                    Some(val) => val,
                    None => bail!(
                        "Malformed output from uniswap getEthToTokenInputPrice call {:?}",
                        tokens_bought
                    ),
                }))
            }),
        )
    }

    /// Price of Dai in Eth
    pub fn dai_to_eth_price(&self, amount: Uint256) -> Box<Future<Item = Uint256, Error = Error>> {
        let web3 = self.eth_web3.clone();
        let uniswap_address = self.uniswap_address.clone();
        let own_address = self.own_address.clone();

        Box::new(
            web3.contract_call(
                uniswap_address,
                "getTokenToEthInputPrice(uint256)",
                &[amount.into()],
                own_address,
            )
            .and_then(move |eth_bought| {
                Ok(Uint256::from_bytes_be(match eth_bought.get(0..32) {
                    Some(val) => val,
                    None => bail!(
                        "Malformed output from uniswap getTokenToEthInputPrice call {:?}",
                        eth_bought
                    ),
                }))
            }),
        )
    }

    /// Sell `eth_amount` ETH for Dai.
    /// This function will error out if it takes longer than 'timeout' and the transaction is guaranteed not
    /// to be accepted on the blockchain after this time.
    pub fn eth_to_dai_swap(
        &self,
        eth_amount: Uint256,
        timeout: u64,
    ) -> Box<Future<Item = Uint256, Error = Error>> {
        let uniswap_address = self.uniswap_address.clone();
        let own_address = self.own_address.clone();
        let secret = self.secret.clone();
        let web3 = self.eth_web3.clone();

        Box::new(
            web3.eth_get_latest_block()
                .join(self.eth_to_dai_price(eth_amount.clone()))
                .and_then(move |(block, expected_dai)| {
                    // Equivalent to `amount * (1 - 0.025)` without using decimals
                    let expected_dai = (expected_dai / 40u64.into()) * 39u64.into();
                    let deadline = block.timestamp + timeout.into();
                    let payload = encode_call(
                        "ethToTokenSwapInput(uint256,uint256)",
                        &[expected_dai.clone().into(), deadline.into()],
                    );

                    web3.send_transaction(
                        uniswap_address,
                        payload,
                        eth_amount,
                        own_address,
                        secret,
                        vec![
                            SendTxOption::GasPriceMultiplier(2u64.into()),
                            SendTxOption::GasLimit(60_000u64.into()),
                        ],
                    )
                    .join(
                        web3.wait_for_event_alt(
                            uniswap_address,
                            "TokenPurchase(address,uint256,uint256)",
                            Some(vec![own_address.into()]),
                            None,
                            None,
                            |_| true,
                        )
                        .timeout(Duration::from_secs(timeout)),
                    )
                    .and_then(move |(_tx, response)| {
                        let transfered_dai = Uint256::from_bytes_be(&response.topics[3]);
                        Ok(transfered_dai)
                    })
                }),
        )
    }

    /// Checks if the uniswap contract has been approved to spend dai from our account.
    pub fn check_if_uniswap_dai_approved(&self) -> Box<Future<Item = bool, Error = Error>> {
        let web3 = self.eth_web3.clone();
        let uniswap_address = self.uniswap_address.clone();
        let dai_address = self.foreign_dai_contract_address.clone();
        let own_address = self.own_address.clone();

        Box::new(
            web3.contract_call(
                dai_address,
                "allowance(address,address)",
                &[own_address.into(), uniswap_address.into()],
                own_address,
            )
            .and_then(move |allowance| {
                let allowance = Uint256::from_bytes_be(match allowance.get(0..32) {
                    Some(val) => val,
                    None => bail!(
                        "Malformed output from uniswap getTokenToEthInputPrice call {:?}",
                        allowance
                    ),
                });

                // Check if the allowance remaining is greater than half of a Uint256- it's as good
                // a test as any.
                Ok(allowance > (Uint256::max_value() / 2u32.into()))
            }),
        )
    }

    /// Sends transaction to the DAI contract to approve uniswap transactions, this future will not
    /// resolve until the process is either successful for the timeout finishes
    pub fn approve_uniswap_dai_transfers(
        &self,
        timeout: Duration,
    ) -> Box<Future<Item = (), Error = Error>> {
        let dai_address = self.foreign_dai_contract_address.clone();
        let own_address = self.own_address.clone();
        let uniswap_address = self.uniswap_address.clone();
        let secret = self.secret.clone();
        let web3 = self.eth_web3.clone();

        let payload = encode_call(
            "approve(address,uint256)",
            &[uniswap_address.into(), Uint256::max_value().into()],
        );

        Box::new(
            web3.send_transaction(
                dai_address,
                payload,
                0u32.into(),
                own_address,
                secret,
                vec![SendTxOption::GasPriceMultiplier(2u64.into())],
            )
            .join(web3.wait_for_event_alt(
                dai_address,
                "Approval(address,address,uint256)",
                Some(vec![own_address.into()]),
                Some(vec![uniswap_address.into()]),
                None,
                |_| true,
            ))
            .timeout(timeout)
            .and_then(move |_| Ok(())),
        )
    }

    /// Sell `dai_amount` Dai for ETH
    /// This function will error out if it takes longer than 'timeout' and the transaction is guaranteed not
    /// to be accepted on the blockchain after this time.
    pub fn dai_to_eth_swap(
        &self,
        dai_amount: Uint256,
        timeout: u64,
    ) -> Box<Future<Item = Uint256, Error = Error>> {
        let uniswap_address = self.uniswap_address.clone();
        let own_address = self.own_address.clone();
        let secret = self.secret.clone();
        let web3 = self.eth_web3.clone();

        Box::new(
            web3.eth_get_latest_block()
                .join(self.dai_to_eth_price(dai_amount.clone()))
                .and_then(move |(block, expected_eth)| {
                    // Equivalent to `amount * (1 - 0.025)` without using decimals
                    let expected_eth = (expected_eth / 40u64.into()) * 39u64.into();
                    let deadline = block.timestamp + timeout.into();
                    let payload = encode_call(
                        "tokenToEthSwapInput(uint256,uint256,uint256)",
                        &[
                            dai_amount.into(),
                            expected_eth.clone().into(),
                            deadline.into(),
                        ],
                    );

                    web3.send_transaction(
                        uniswap_address,
                        payload,
                        0u32.into(),
                        own_address,
                        secret,
                        vec![
                            SendTxOption::GasPriceMultiplier(2u64.into()),
                            SendTxOption::GasLimit(60_000u64.into()),
                        ],
                    )
                    .join(
                        web3.wait_for_event_alt(
                            uniswap_address,
                            "EthPurchase(address,uint256,uint256)",
                            Some(vec![own_address.into()]),
                            None,
                            None,
                            |_| true,
                        )
                        .timeout(Duration::from_secs(timeout)),
                    )
                    .and_then(move |(_tx, response)| {
                        let transfered_eth = Uint256::from_bytes_be(&response.topics[3]);
                        Ok(transfered_eth)
                    })
                }),
        )
    }

    /// Bridge `dai_amount` dai to xdai
    pub fn dai_to_xdai_bridge(
        &self,
        dai_amount: Uint256,
    ) -> Box<Future<Item = Uint256, Error = Error>> {
        let eth_web3 = self.eth_web3.clone();
        let foreign_dai_contract_address = self.foreign_dai_contract_address.clone();
        let xdai_foreign_bridge_address = self.xdai_foreign_bridge_address.clone();
        let own_address = self.own_address.clone();
        let secret = self.secret.clone();

        // You basically just send it some coins
        // We have no idea when this has succeeded since the events are not indexed
        Box::new(eth_web3.send_transaction(
            foreign_dai_contract_address,
            encode_call(
                "transfer(address,uint256)",
                &[
                    xdai_foreign_bridge_address.into(),
                    dai_amount.clone().into(),
                ],
            ),
            0u32.into(),
            own_address,
            secret,
            vec![],
        ))
    }

    /// Bridge `xdai_amount` xdai to dai
    pub fn xdai_to_dai_bridge(
        &self,
        xdai_amount: Uint256,
    ) -> Box<Future<Item = Uint256, Error = Error>> {
        let xdai_web3 = self.xdai_web3.clone();

        let xdai_home_bridge_address = self.xdai_home_bridge_address.clone();

        let own_address = self.own_address.clone();
        let secret = self.secret.clone();

        // You basically just send it some coins
        Box::new(xdai_web3.send_transaction(
            xdai_home_bridge_address,
            Vec::new(),
            xdai_amount,
            own_address,
            secret,
            vec![
                SendTxOption::GasPrice(10_000_000_000u128.into()),
                SendTxOption::NetworkId(100u64),
            ],
        ))
    }

    pub fn get_dai_balance(&self, address: Address) -> Box<Future<Item = Uint256, Error = Error>> {
        let web3 = self.eth_web3.clone();
        let dai_address = self.foreign_dai_contract_address;
        let own_address = self.own_address;
        Box::new(
            web3.contract_call(
                dai_address,
                "balanceOf(address)",
                &[address.into()],
                own_address,
            )
            .and_then(|balance| {
                Ok(Uint256::from_bytes_be(match balance.get(0..32) {
                    Some(val) => val,
                    None => bail!(
                        "Got bad output for DAI balance from the full node {:?}",
                        balance
                    ),
                }))
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix;
    use std::str::FromStr;

    fn new_token_bridge() -> TokenBridge {
        let pk = PrivateKey::from_str(&format!(
            "FE1FC0A7A29503BAF72274A{}601D67309E8F3{}D22",
            "AA3ECDE6DB3E20", "29F7AB4BA52"
        ))
        .unwrap();

        TokenBridge::new(
            Address::from_str("0x09cabEC1eAd1c0Ba254B09efb3EE13841712bE14".into()).unwrap(),
            Address::from_str("0x7301CFA0e1756B71869E93d4e4Dca5c7d0eb0AA6".into()).unwrap(),
            Address::from_str("0x4aa42145Aa6Ebf72e164C9bBC74fbD3788045016".into()).unwrap(),
            Address::from_str("0x89d24A6b4CcB1B6fAA2625fE562bDD9a23260359".into()).unwrap(),
            Address::from_str("0x79AE13432950bF5CDC3499f8d4Cf5963c3F0d42c".into()).unwrap(),
            pk,
            "https://eth.althea.org".into(),
            "https://dai.althea.org".into(),
        )
    }

    fn eth_to_wei(eth: f64) -> Uint256 {
        let wei = (eth * 1000000000000000000f64) as u64;
        wei.into()
    }

    #[test]
    fn test_eth_to_dai_swap() {
        let system = actix::System::new("test");

        let token_bridge = new_token_bridge();

        actix::spawn(
            token_bridge
                .dai_to_eth_price(eth_to_wei(0.01f64))
                .and_then(move |one_cent_in_eth| {
                    token_bridge.eth_to_dai_swap(one_cent_in_eth.clone(), 600)
                })
                .then(|res| {
                    res.unwrap();
                    actix::System::current().stop();
                    Box::new(futures::future::ok(()))
                }),
        );

        system.run();
    }

    #[test]
    fn test_dai_to_eth_swap() {
        let system = actix::System::new("test");
        let token_bridge = new_token_bridge();

        actix::spawn(
            token_bridge
                .approve_uniswap_dai_transfers(Duration::from_secs(600))
                .and_then(move |_| token_bridge.dai_to_eth_swap(eth_to_wei(0.01f64), 600))
                .then(|res| {
                    res.unwrap();
                    actix::System::current().stop();
                    Box::new(futures::future::ok(()))
                }),
        );

        system.run();
    }

    #[test]
    fn test_dai_to_xdai_bridge() {
        let system = actix::System::new("test");

        let token_bridge = new_token_bridge();

        actix::spawn(
            token_bridge
                // All we can really do here is test that it doesn't throw. Check your balances in
                // 5-10 minutes to see if the money got transferred.
                .dai_to_xdai_bridge(eth_to_wei(0.01f64))
                .then(|res| {
                    res.unwrap();
                    actix::System::current().stop();
                    Box::new(futures::future::ok(()))
                }),
        );

        system.run();
    }

    #[test]
    fn test_xdai_to_dai_bridge() {
        let system = actix::System::new("test");

        let token_bridge = new_token_bridge();

        actix::spawn(
            token_bridge
                // All we can really do here is test that it doesn't throw. Check your balances in
                // 5-10 minutes to see if the money got transferred.
                .xdai_to_dai_bridge(eth_to_wei(0.01f64))
                .then(|res| {
                    res.unwrap();
                    actix::System::current().stop();
                    Box::new(futures::future::ok(()))
                }),
        );

        system.run();
    }
}
