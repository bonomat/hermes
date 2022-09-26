const config = {
    "packagerConfig": {
        "name": "ItchySats Desktop",
        "appBundleId": "com.electron.itchysats",
        "icon": "images/icon",
        "overwrite": true,
        // "osxNotarize": {
        //     "appleId": "philipp@hoenisch.at",
        //     "appleIdPassword": "Slavery-Paladin-huarache-1"
        // },
        "osxSign": {
            "identity": "Developer ID Application: Philipp Hoenisch (V3D64P9D6W)",
            "hardened-runtime": true,
            "entitlements": "static/entitlements.plist",
            "entitlements-inherit": "static/entitlements.plist",
            "signature-flags": "library",
        },
    },
    "makers": [
        {
            "name": "@electron-forge/maker-squirrel",
            "config": {
                "name": "taker_electron",
                "setupIcon": "images/icon.ico",
            },
        },
        {
            "name": "@electron-forge/maker-zip",
            "platforms": [
                "darwin",
            ],
        },
        // {
        //     "name": "@electron-forge/maker-dmg",
        // },
    ],
    "publishers": [
        {
            "name": "@electron-forge/publisher-github",
            "config": {
                // todo: change to itchysats/itchysats
                "repository": {
                    "owner": "bonomat",
                    "name": "hermes",
                },
                "icon": "images/icon.icns",
            },
        },
    ],
};

function notarizeMaybe() {
    if (process.platform !== "darwin") {
        return;
    }

    // if (!process.env.CI) {
    //     console.log(`Not in CI, skipping notarization`);
    //     return;
    // }

    if (!process.env.APPLE_ID || !process.env.APPLE_ID_PASSWORD) {
        console.warn(
            "Should be notarizing, but environment variables APPLE_ID or APPLE_ID_PASSWORD are missing!",
        );
        return;
    }

    config.packagerConfig.osxNotarize = {
        appBundleId: "com.electron.itchysats",
        appleId: process.env.APPLE_ID,
        appleIdPassword: process.env.APPLE_ID_PASSWORD,
        ascProvider: "LT94ZKYDCJ",
    };
}

notarizeMaybe();

module.exports = config;
