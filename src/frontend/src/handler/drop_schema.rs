// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::error::{ErrorCode, Result, TrackingIssue};
use risingwave_sqlparser::ast::{AstOption, DropMode, ObjectName};

use crate::binder::Binder;
use crate::catalog::CatalogError;
use crate::session::OptimizerContext;

pub async fn handle_drop_schema(
    context: OptimizerContext,
    schema_name: ObjectName,
    if_exist: bool,
    mode: AstOption<DropMode>,
) -> Result<PgResponse> {
    let session = context.session_ctx;
    let catalog_reader = session.env().catalog_reader();
    let (database_name, schema_name) =
        Binder::resolve_schema_name(session.database(), schema_name)?;

    let schema = {
        let reader = catalog_reader.read_guard();
        match reader.get_schema_by_name(&database_name, &schema_name) {
            Ok(schema) => schema.clone(),
            Err(err) => {
                // If `if_exist` is true, not return error.
                if if_exist {
                    return Ok(PgResponse::empty_result(StatementType::DROP_SCHEMA));
                } else {
                    return Err(err);
                }
            }
        }
    };
    let schema_id = {
        // If the mode is `Restrict` or `None`, the `schema` need to be empty.
        if AstOption::Some(DropMode::Restrict) == mode || AstOption::None == mode {
            if !schema.is_empty() {
                return Err(CatalogError::NotEmpty("schema", schema_name).into());
            }
            schema.id()
        } else {
            return Err(ErrorCode::NotImplemented(
                format!("unsupported drop mode: {}", mode),
                TrackingIssue::none(),
            )
            .into());
        }
    };

    let catalog_writer = session.env().catalog_writer();
    catalog_writer.drop_schema(schema_id).await?;
    Ok(PgResponse::empty_result(StatementType::DROP_SCHEMA))
}

#[cfg(test)]
mod tests {
    use crate::test_utils::LocalFrontend;

    #[tokio::test]
    async fn test_drop_schema() {
        let frontend = LocalFrontend::new(Default::default()).await;
        let session = frontend.session_ref();
        let catalog_reader = session.env().catalog_reader();

        frontend.run_sql("CREATE SCHEMA schema").await.unwrap();

        frontend.run_sql("CREATE TABLE schema.table").await.unwrap();

        assert!(frontend.run_sql("DROP SCHEMA schema").await.is_err());

        frontend.run_sql("DROP TABLE schema.table").await.unwrap();

        frontend.run_sql("DROP SCHEMA schema").await.unwrap();

        let schema = catalog_reader
            .read_guard()
            .get_database_by_name("schema")
            .ok()
            .cloned();
        assert!(schema.is_none());
    }
}
