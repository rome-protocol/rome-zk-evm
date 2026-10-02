//! Expected intermediate values, copied mechanically from specification section 9.3.
#![allow(dead_code)]

pub struct Challenges {
    pub beta: &'static str,
    pub gamma: &'static str,
    pub alpha: &'static str,
    pub z: &'static str,
    pub v: &'static str,
    pub u: &'static str,
}
pub const CHALLENGES: [Challenges; 4] = [
    Challenges {
        beta: "27f2e8398d219e570949239d05f1abd7899cfe63b0a4caf35a4df5f80a2b0295",
        gamma: "28979c647a4ac45f88534b292ba487eff23d14cde1564083c96d3c34a466f6e5",
        alpha: "2cae1e79d14d7e84f5a00b2e409a8cc9a76e11a491680fd226fc4abdf5d27785",
        z: "014626e37fc528e3454c247ee5801a867a5572f218c446ec084101df11926571",
        v: "2f13f4cb1a8603e15a64d8a7c4b63df670f24eaa13a233a8f4a820c0ce04e2d3",
        u: "2b847850afcc8ae92183de3d516e5ef871be0755eca134b1199e21815587bb12",
    },
    Challenges {
        beta: "137569e99cbcecd01340295ebb8885cc4eac8775895c0e6207e443d17bfbe85b",
        gamma: "05a725fc811fa9f4a204482389fb0c52deee8a3c82b4b07db2939e8acfca906a",
        alpha: "088bfa0a4c8dbe9764d2730f8a4fc66c635bfaec5007a36bef80e03b9d326d7f",
        z: "08e865ac0816e1d47090499a73b287fd2c0d774be208fc3faf57f9485c48bfc1",
        v: "1aafaf3c4340d6f956e63a2deaf643c417154661182fdead286fd2d19146fab1",
        u: "2a525e523f463a1150d98e5b4bddab3952847a263cde0b5cc1d57f2961a3003f",
    },
    Challenges {
        beta: "2b95be8d12255ddef8eda3f6ae80cd773b7fbde5165278b7d7378c39fa282921",
        gamma: "1093f548245e3ba6a0770a30eb8821757b76822f67a6786eacecd66822980531",
        alpha: "11101c53a4679488a657b8256148963256b977c48b66fb5f2ca562b6bbbe52f2",
        z: "2abf0cecfb3af64f61104bda4c05181cdf3a2acd95ce057995fd4c3326f97b76",
        v: "05741e51564fcc7f11a38fa2add1975b09d53ac462c3613aab9ad48b3fdef40f",
        u: "2db9f103b3bfeac739ace7d7a2589da225500f345762d03f3f7b3ec298ab40ed",
    },
    Challenges {
        beta: "247f10322172612dca9ef753c3b003dddcab8fc7272f7dc2d7203352802572ef",
        gamma: "23e5935ffc61a36b71595733cd18e785954f675b047152627dfc70547677f886",
        alpha: "09dacb5c4a52c131489313b5b9e6f41fcaf17daac7df757380645b5f92246e6b",
        z: "1b468b9fff07744e942d2d48e705d94f5fa26372955586fda4f9e519c4fd72bc",
        v: "11f86f39ba4e7f4c289a72ad9ecac7a15a9263069aaf31abc373bcd663470382",
        u: "04ea9ba38595107f59ff760dec850eeb067b457d8a27205962138c99f35a2580",
    },
];

pub const B14_ZN: &str = "19c59647c9e2c3a5088ca502bee782be6d0e21bf7f04c19d84f0f03148c782ee";
pub const B14_ZH: &str = "19c59647c9e2c3a5088ca502bee782be6d0e21bf7f04c19d84f0f03148c782ed";
pub const B14_L1: &str = "08d05d1b4ab56fe9a724c16f78df577662c131111860e409600385dee3f67912";
pub const B14_PI: &str = "2304450e4c3032f86fe038c6037c5788cfec7e9edd9bff95fd24d6237c547459";
pub const B14_R0: &str = "0c354ce6fa10ac921d446e964909d8e1de9914b70d4043461db8732974cb89b2";
pub const B14_EE: &str = "020d863ba4d22bfae20b66222500cfbb54f1001e14ff87073ba5a19af75dba6e";
pub const B14_D: (&str, &str) = (
    "15dfae729d1d7205a04d95ee221ecbc28ee928f0b55dd7a156bc80315997525d",
    "0158ad913915cb8508c6f376623c1e9ec6e43978da2e7c3df2493488e668c397",
);
pub const B14_F: (&str, &str) = (
    "0a0a117f67e7aa84d6cf80e9f9bfe80a549858c47d402601090119085ccb6e5e",
    "2ffacba74133bb63cf7496f9e674a364b66fe6a5f6f0f27a30efee23576cde7e",
);
pub const B14_E: (&str, &str) = (
    "234405f76f98f412421c168f7f807862318b54f9910a99702a06e44acb4ab616",
    "1744893107feca2b1b371f5b61788ac0b2dcef2ae5f65c313fc5f33fd64b62c8",
);
pub const B14_PL: (&str, &str) = (
    "04df059f617dd6a055c6955a51af11d346a4cddc2933fab4b20eae9c679b5847",
    "12d366fb0d5a5a09f07128bdca1240b5ee468bfcd9324deb5721b6c489242272",
);
pub const B14_PR: (&str, &str) = (
    "1411508adc21fadab6385c888816ca753353b1fbb9b3670f5aae08daf1c618b8",
    "120184316fd495db414691424240aebf7990ecf926f3039b1f81ff8367d7afd5",
);

/// (P_L, P_R) per fixture in the order block 14, 166, 169, reset-6.
/// A G1 point as two hex strings, x then y.
pub type Pt = (&'static str, &'static str);
pub const PAIRS: [(Pt, Pt); 4] = [
    ((B14_PL.0, B14_PL.1), (B14_PR.0, B14_PR.1)),
    (
        (
            "06abfb7d7a4c02eff6905e502580368fa90394fdea886673b77e4c782c804fe5",
            "0107ff7e20889e474bbae0f73982d5ca4506605f078f8d0da2f2d11bf38758e3",
        ),
        (
            "23a8a3cc25e6b6d64c070b5a468844821a257128a86f521395c816e35346ad8c",
            "0159338bac051fa01cba25cbd68952819ec1abdf6d74f1f12020974786e579ed",
        ),
    ),
    (
        (
            "0cc3c95ef2df16f327b60d5ba9ca10d893747be0b5f35960dcf8142dfc94c409",
            "2b7401c5a1302674706b91b75b75669fb5b4580c43a5b002711f3d065bf6796d",
        ),
        (
            "184507ce81bfc41a90fbe816b6060cdbeacabfb7a02fd763b0e948dc8919041b",
            "06be532c261fa7b12d324e837ac82cdbae9c40d4bfae7232d67bb7cfc7546842",
        ),
    ),
    (
        (
            "1cc920324f9dd574ac6c0094ee109852ee1b6beec2bc0f6b87ac6b6acf661402",
            "15a2cfd4ca9c6ca5093b3b92263677f353d3a72244449c8da4cf12c04a300d55",
        ),
        (
            "2c50bb62bbaa1f285852489ca222665d493b1e1a67fe4c41143c850ba0e8da8b",
            "1145918db1dedd256979fb6788100740f9e5111a2ee203821f3eeae6d7035331",
        ),
    ),
];
