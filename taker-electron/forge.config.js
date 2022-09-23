module.exports = {
    "packagerConfig": {
        "name": "ItchySats Desktop",
        "appBundleId": "ItchySats",
        "icon": "logo.icns",
        "overwrite": true,
        "appVersion": "0.6.1",
    },
    "makers": [
        {
            "name": "@electron-forge/maker-squirrel",
            "config": {
                "name": "taker_electron",
            },
        },
        {
            "name": "@electron-forge/maker-zip",
            "platforms": [
                "darwin",
            ],
        },
        {
            "name": "@electron-forge/maker-deb",
            "config": {},
        },
        {
            "name": "@electron-forge/maker-rpm",
            "config": {},
        },
    ],
};
