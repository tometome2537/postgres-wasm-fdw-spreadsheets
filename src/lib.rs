// コンパイラに対して警告を無視するように指示
#[allow(warnings)]
// ⭐️ここからモジュールとインポート


mod bindings;
// serde_json::ValueをJsonValueとしてインポートし、JSONデータを扱います。
use serde_json::Value as JsonValue;
use serde_json::Value;
use serde_json::to_string; // to_string関数をインポートします
use serde_json::json;
// bindingsモジュールを使用し、FDWの操作に必要なサポート機能をインポートしています。
use bindings::{
    exports::supabase::wrappers::routines::Guest,
    supabase::wrappers::{
        http, time,
        types::{Cell, Context, FdwError, FdwResult, OptionsType, Row, TypeOid},
        utils,
    },
};

// デフォルト追加モジュール
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::error::Error;
// 追加モジュール
use serde::{Deserialize, Serialize};
use tokio::runtime::Builder;




// ⭐️ここまでモジュールとインポート


// Rustのderive属性を使用して、構造体や列挙体に対して特定のトレイトの実装を自動的に生成するものです。
// structは構造体。classみたいな。ここで定義されるのはclass内のメソッドを定義するようなもの。this.○○○で呼び出す。
#[derive(Debug, Default)]
struct SpreadsheetsFdw {
    base_url: String, // APIのベースURL。
    src_rows: Vec<JsonValue>, // 取得したデータのJSON配列。
    src_idx: usize, // 現在のスキャン位置を示すインデックス。
}


// INSTANCEは、↑の構造体のシングルトンインスタンスを指すポインタです。unsafeブロックでアクセスされるため、スレッドセーフではありません。
static mut INSTANCE: *mut SpreadsheetsFdw = std::ptr::null_mut::<SpreadsheetsFdw>();

// implは構造体に機能を追加するために使用される。classにメソッドを追加するようなイメージ。
impl SpreadsheetsFdw {
    // SpreadsheetsFdwのシングルトンインスタンスを初期化します。
    fn init_instance() {
        let instance = Self::default();
        unsafe {
            INSTANCE = Box::leak(Box::new(instance));
        }
    }

    // シングルトンインスタンスへのミュータブルな参照を取得します。
    fn this_mut() -> &'static mut Self {
        unsafe { &mut (*INSTANCE) }
    }
}

// SpreadsheetsFdw構造体に対してGuestトレイトを実装しています。
// GuestトレイトはFDWの各種操作に対応するためのインターフェースを提供しており、
// これにより外部データソースをPostgreSQLに統合するための機能を定義します。以下に、各メソッドの説明を示します。
impl Guest for SpreadsheetsFdw {
    fn host_version_requirement() -> String {
        // semver expression for Wasm FDW host version requirement
        // ref: https://docs.rs/semver/latest/semver/enum.Op.html
        "^0.1.0".to_string()
    }

    // 初期化
    fn init(ctx: &Context) -> FdwResult {
        Self::init_instance();
        let this = Self::this_mut();
        // 外部サーバーオプションからAPI URLを取得する（指定されている場合）
        let opts = ctx.get_options(OptionsType::Server);

        // let service_account = opts.require("service_account")?;
    
        this.base_url = opts.require_or("base_url", "https://docs.google.com/spreadsheets/d");
        Ok(())
    }

    // データスキャンの開始時に行う準備作業を担当します。具体的には、ソースデータの取得や初期化処理などを行います。
    fn begin_scan(ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
         // ↓ SQLのスキーマで渡されたoptionの値を読み込む。
         let opts = ctx.get_options(OptionsType::Table);

         let spread_sheet_id = opts.require("spread_sheet_id")?;
         let sheet_id = opts.get("sheet_id");

         // URLを組み立てる。
         let url = format!("{}/{}/gviz/tq?tqx=out:json", this.base_url, spread_sheet_id,);

        // sheet_idが定義されている場合のURLを組み立てる。
         let url = match sheet_id {
            Some(sheet_id) => format!(
                "{}/{}/gviz/tq?gid={}&tqx=out:json",
                this.base_url, spread_sheet_id, sheet_id,
            ),
            None => format!("{}/{}/gviz/tq?tqx=out:json", this.base_url, spread_sheet_id,),
        };

        // API通信のためのヘッダーを定義
        let headers: Vec<(String, String)> = vec![
            ("user-agent".to_owned(), "Sheets FDW".to_owned()),
            // header to make JSON response more cleaner
            ("x-datasource-auth".to_owned(), "true".to_owned()),
        ];

        // Google API にリクエストを送り、レスポンスを JSON として解析する
        let req = http::Request {
            method: http::Method::Get,
            url,
            headers,
            body: String::default(),
        };
        let resp = http::get(&req)?;
        // 無効なプレフィックスをレスポンスから削除して、有効なJSON文字列にする。
        let body = resp.body.strip_prefix(")]}'\n").ok_or("invalid response")?;
        let resp_json: JsonValue = serde_json::from_str(body).map_err(|e| e.to_string())?;
        // レスポンスからソースの行を抽出する
        this.src_rows = resp_json
            .pointer("/table/rows")
            .ok_or("cannot get rows from response")
            .map(|v| v.as_array().unwrap().to_owned())?;
        // Postgres INFO をユーザーに出力する（psql で表示可能）、デバッグにも便利
        utils::report_info(&format!(
            "We got response array length: {}",
            this.src_rows.len()
        ));
        Ok(())
    }

    // この関数 iter_scan は、PostgreSQLのFDW（Foreign Data Wrapper）におけるデータスキャンの処理を行う部分です。
    // ここでは、外部データソースからデータを取得し、PostgreSQLに対して返すための変換を行います。
    fn iter_scan(ctx: &Context, row: &Row) -> Result<Option<u32>, FdwError> {
        let this = Self::this_mut();
        // if all source rows are consumed, stop data scan
        if this.src_idx >= this.src_rows.len() {
            return Ok(None);
        }
        // extract current source row, an example of the source row in JSON:
        // {
        //   "c": [{
        //      "v": 1.0,
        //      "f": "1"
        //    }, {
        //      "v": "Erlich Bachman"
        //    }, null, null, null, null, { "v": null }
        //    ]
        // }
        let src_row = &this.src_rows[this.src_idx];
        // loop through each target column, map source cell to target cell
        for tgt_col in ctx.get_columns() {
            let (tgt_col_num, tgt_col_name) = (tgt_col.num(), tgt_col.name());
            if let Some(src) = src_row.pointer(&format!("/c/{}/v", tgt_col_num - 1)) {
                // we only support I64 and String cell types here, add more type
                // conversions if you need
                let cell = match tgt_col.type_oid() {
                    TypeOid::I64 => src.as_f64().map(|v| Cell::I64(v as _)),
                    TypeOid::String => src.as_str().map(|v| Cell::String(v.to_owned())),
                    _ => {
                        return Err(format!(
                            "column {} data type is not supported",
                            tgt_col_name
                        ));
                    }
                };
                // push the cell to target row
                row.push(cell.as_ref());
            } else {
                row.push(None);
            }
        }
        // advance to next source row
        this.src_idx += 1;
        // tell Postgres we've done one row, and need to scan the next row
        Ok(Some(0))
    }

    // ここからエラーと未サポート機能の関数。

    fn re_scan(_ctx: &Context) -> FdwResult {
        Err("re_scan on foreign table is not supported".to_owned())
    }

    fn end_scan(_ctx: &Context) -> FdwResult {
        let this = Self::this_mut();
        this.src_rows.clear();
        Ok(())
    }

    fn begin_modify(_ctx: &Context) -> FdwResult {
        Err("modify on foreign table is not supported".to_owned())
    }

    fn insert(_ctx: &Context, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn update(_ctx: &Context, _rowid: Cell, _row: &Row) -> FdwResult {
        Ok(())
    }

    fn delete(_ctx: &Context, _rowid: Cell) -> FdwResult {
        Ok(())
    }

    fn end_modify(_ctx: &Context) -> FdwResult {
        Ok(())
    }
}

// SpreadsheetsFdwをFDWとしてエクスポートしています。
bindings::export!(SpreadsheetsFdw with_types_in bindings);
