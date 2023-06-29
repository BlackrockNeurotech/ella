use arrow_flight::decode::FlightRecordBatchStream;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::FlightError;
use arrow_flight::sql::metadata::SqlInfoList;
use arrow_flight::sql::{
    server::FlightSqlService, ActionBeginSavepointRequest, ActionBeginSavepointResult,
    ActionBeginTransactionRequest, ActionBeginTransactionResult, ActionCancelQueryRequest,
    ActionCancelQueryResult, ActionClosePreparedStatementRequest,
    ActionCreatePreparedStatementRequest, ActionCreatePreparedStatementResult,
    ActionCreatePreparedSubstraitPlanRequest, ActionEndSavepointRequest,
    ActionEndTransactionRequest, Any, CommandGetCatalogs, CommandGetCrossReference,
    CommandGetDbSchemas, CommandGetExportedKeys, CommandGetImportedKeys, CommandGetPrimaryKeys,
    CommandGetSqlInfo, CommandGetTableTypes, CommandGetTables, CommandGetXdbcTypeInfo,
    CommandPreparedStatementQuery, CommandPreparedStatementUpdate, CommandStatementQuery,
    CommandStatementSubstraitPlan, CommandStatementUpdate, ProstMessageExt, SqlInfo,
    TicketStatementQuery,
};
use arrow_flight::{
    flight_service_server::FlightService, Action, FlightData, FlightDescriptor, FlightEndpoint,
    FlightInfo, HandshakeRequest, HandshakeResponse, IpcMessage, SchemaAsIpc, Ticket,
};
use datafusion::arrow::ipc::writer::IpcWriteOptions;
use datafusion::sql::parser::{CopyToSource, CopyToStatement, Statement};
use datafusion::sql::sqlparser::ast::{Ident, ObjectName};
use futures::{SinkExt, Stream, TryStreamExt};
use once_cell::sync::Lazy;
use prost::bytes::Bytes;
use prost::Message;
use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status, Streaming};

use synapse_engine::Engine;

use crate::prepare::{PreparedStatement, PreparedStatements};
use crate::ticket::{SynapseTicket, TicketTracker};

macro_rules! status {
    ($desc:expr, $err:expr) => {
        Status::internal(format!("{}: {} at {}:{}", $desc, $err, file!(), line!()))
    };
}

static SQL_INFO: Lazy<SqlInfoList> = Lazy::new(|| {
    SqlInfoList::new()
        .with_sql_info(SqlInfo::FlightSqlServerName, "synapse")
        .with_sql_info(SqlInfo::FlightSqlServerVersion, env!("CARGO_PKG_VERSION"))
        // https://github.com/apache/arrow/blob/f9324b79bf4fc1ec7e97b32e3cce16e75ef0f5e3/format/Schema.fbs#L24
        .with_sql_info(SqlInfo::FlightSqlServerArrowVersion, "1.3")
});

#[derive(Debug, Clone)]
pub struct SynapseSqlService {
    engine: Engine,
    tickets: TicketTracker,
    statements: PreparedStatements,
}

impl SynapseSqlService {
    pub fn new(engine: Engine) -> Self {
        let ctx = engine.ctx().session().clone();
        let tickets = TicketTracker::new(ctx.clone());
        let statements = PreparedStatements::new(ctx);
        Self {
            engine,
            tickets,
            statements,
        }
    }
}

impl SynapseSqlService {
    async fn take_ticket(
        &self,
        ticket: &SynapseTicket,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        match self.tickets.take(ticket) {
            Some(task) => {
                let stream = task
                    .stream()
                    .await?
                    .map_err(|err| FlightError::ExternalError(Box::new(err)));

                let stream = FlightDataEncoderBuilder::new()
                    .with_schema(task.schema())
                    .build(stream)
                    .map_err(Into::into);

                Ok(Response::new(Box::pin(stream)))
            }
            None => Err(Status::not_found(format!(
                "ticket {:?} does not exist or has already been used",
                ticket
            ))),
        }
    }

    async fn sql_query(&self, query: &str) -> crate::Result<FlightInfo> {
        let (ticket, task) = self.tickets.put_sql(query).await?;
        let ticket = TicketStatementQuery {
            statement_handle: ticket.into(),
        };
        let endpoint = FlightEndpoint {
            ticket: Some(Ticket {
                ticket: ticket.as_any().encode_to_vec().into(),
            }),
            location: vec![],
        };

        let mut info = FlightInfo::new()
            .try_with_schema(&task.schema())?
            .with_endpoint(endpoint)
            .with_ordered(task.is_ordered());

        if let Some(rows) = task.num_rows() {
            info = info.with_total_records(rows as i64);
        }
        if let Some(bytes) = task.byte_size() {
            info = info.with_total_bytes(bytes as i64);
        }

        Ok(info)
    }
}

#[tonic::async_trait]
impl FlightSqlService for SynapseSqlService {
    type FlightService = SynapseSqlService;

    async fn do_handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>>,
        Status,
    > {
        let result = HandshakeResponse {
            protocol_version: 0,
            payload: Default::default(),
        };
        let result = Ok(result);
        let output = futures::stream::iter(vec![result]);
        return Ok(Response::new(Box::pin(output)));
    }

    #[tracing::instrument(skip(self, _message))]
    async fn do_get_fallback(
        &self,
        request: Request<Ticket>,
        _message: Any,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let ticket = request.get_ref().clone().try_into()?;
        self.take_ticket(&ticket).await
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_flight_info_statement(
        &self,
        query: CommandStatementQuery,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let info = self
            .sql_query(&query.query)
            .await?
            .with_descriptor(request.into_inner());
        Ok(Response::new(info))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_substrait_plan(
        &self,
        _query: CommandStatementSubstraitPlan,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_substrait_plan not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_prepared_statement(
        &self,
        _cmd: CommandPreparedStatementQuery,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_prepared_statement not implemented",
        ))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_flight_info_catalogs(
        &self,
        query: CommandGetCatalogs,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let flight_descriptor = request.into_inner();
        let ticket = Ticket {
            ticket: query.as_any().encode_to_vec().into(),
        };
        let endpoint = FlightEndpoint::new().with_ticket(ticket);

        let flight_info = FlightInfo::new()
            .try_with_schema(&query.into_builder().schema())
            .map_err(|e| status!("Unable to encode schema", e))?
            .with_endpoint(endpoint)
            .with_descriptor(flight_descriptor);

        Ok(tonic::Response::new(flight_info))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_flight_info_schemas(
        &self,
        query: CommandGetDbSchemas,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let flight_descriptor = request.into_inner();
        let ticket = Ticket {
            ticket: query.as_any().encode_to_vec().into(),
        };
        let endpoint = FlightEndpoint::new().with_ticket(ticket);

        let flight_info = FlightInfo::new()
            .try_with_schema(&query.into_builder().schema())
            .map_err(|e| status!("Unable to encode schema", e))?
            .with_endpoint(endpoint)
            .with_descriptor(flight_descriptor);

        Ok(tonic::Response::new(flight_info))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_flight_info_tables(
        &self,
        query: CommandGetTables,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let flight_descriptor = request.into_inner();
        let ticket = Ticket {
            ticket: query.as_any().encode_to_vec().into(),
        };
        let endpoint = FlightEndpoint::new().with_ticket(ticket);

        let flight_info = FlightInfo::new()
            .try_with_schema(&query.into_builder().schema())
            .map_err(|e| status!("Unable to encode schema", e))?
            .with_endpoint(endpoint)
            .with_descriptor(flight_descriptor);

        Ok(tonic::Response::new(flight_info))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_table_types not implemented",
        ))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_flight_info_sql_info(
        &self,
        query: CommandGetSqlInfo,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let flight_descriptor = request.into_inner();
        let ticket = Ticket::new(query.as_any().encode_to_vec());
        let endpoint = FlightEndpoint::new().with_ticket(ticket);

        let flight_info = FlightInfo::new()
            .try_with_schema(SqlInfoList::schema())
            .map_err(|e| status!("Unable to encode schema", e))?
            .with_endpoint(endpoint)
            .with_descriptor(flight_descriptor);

        Ok(tonic::Response::new(flight_info))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_primary_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_exported_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_imported_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_imported_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn get_flight_info_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented(
            "get_flight_info_xdbc_type_info not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_statement(
        &self,
        ticket: TicketStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let ticket = SynapseTicket::from_bytes(ticket.statement_handle)?;
        self.take_ticket(&ticket).await
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_prepared_statement(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented(
            "do_get_prepared_statement not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_catalogs(
        &self,
        query: CommandGetCatalogs,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let mut builder = query.into_builder();
        for catalog in self.engine.ctx().session().catalog_names() {
            builder.append(catalog);
        }
        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_schemas(
        &self,
        query: CommandGetDbSchemas,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let mut builder = query.into_builder();

        let ctx = self.engine.ctx().session();
        for name in ctx.catalog_names() {
            let catalog = ctx.catalog(&name).unwrap();
            for schema in catalog.schema_names() {
                builder.append(name.clone(), schema);
            }
        }

        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_tables(
        &self,
        query: CommandGetTables,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let ctx = self.engine.ctx().session();

        let mut builder = query.into_builder();
        for catalog_name in ctx.catalog_names() {
            let catalog = ctx.catalog(&catalog_name).unwrap();
            for schema_name in catalog.schema_names() {
                let schema = catalog.schema(&schema_name).unwrap();
                for table_name in schema.table_names() {
                    let table = schema.table(&table_name).await.unwrap();
                    builder
                        .append(
                            catalog_name.clone(),
                            schema_name.clone(),
                            table_name,
                            "TABLE",
                            &table.schema(),
                        )
                        .map_err(|e| status!("Failed to serialize table info", e))?;
                }
            }
        }

        let schema = builder.schema();
        let batch = builder.build();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented("do_get_table_types not implemented"))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_sql_info(
        &self,
        query: CommandGetSqlInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        let batch = SQL_INFO.filter(&query.info).encode();
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(Arc::new(SqlInfoList::schema().clone()))
            .build(futures::stream::once(async { batch }))
            .map_err(Status::from);
        Ok(Response::new(Box::pin(stream)))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented("do_get_primary_keys not implemented"))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented(
            "do_get_exported_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented(
            "do_get_imported_keys not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented(
            "do_get_cross_reference not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_get_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        Err(Status::unimplemented(
            "do_get_xdbc_type_info not implemented",
        ))
    }

    #[tracing::instrument(skip(self, request))]
    async fn do_put_statement_update(
        &self,
        ticket: CommandStatementUpdate,
        request: Request<Streaming<FlightData>>,
    ) -> Result<i64, Status> {
        let state = self.engine.ctx().session().state();
        let stmt = state
            .sql_to_statement(&ticket.query, &state.config().options().sql_parser.dialect)
            .map_err(crate::Error::from)?;
        match stmt {
            Statement::CopyTo(CopyToStatement {
                source: CopyToSource::Relation(ObjectName(idents)),
                target,
                ..
            }) if &idents[..] == &[Ident::new("this")] => {
                let mut stream = FlightRecordBatchStream::new_from_flight_data(
                    request.into_inner().map_err(Into::into),
                );
                let mut pb = self.engine.topic(target).get().unwrap().publish();
                let mut rows = 0;
                while let Some(batch) = stream.try_next().await? {
                    rows += batch.num_rows();
                    pb.send(batch).await?;
                }
                pb.flush().await?;
                Ok(rows as i64)
            }
            _ => {
                todo!()
            }
        }
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_put_substrait_plan(
        &self,
        _ticket: CommandStatementSubstraitPlan,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<i64, Status> {
        Err(Status::unimplemented(
            "do_put_substrait_plan not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_put_prepared_statement_query(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<<Self as FlightService>::DoPutStream>, Status> {
        Err(Status::unimplemented(
            "do_put_prepared_statement_query not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_put_prepared_statement_update(
        &self,
        _query: CommandPreparedStatementUpdate,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<i64, Status> {
        Err(Status::unimplemented(
            "do_put_prepared_statement_update not implemented",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_create_prepared_statement(
        &self,
        query: ActionCreatePreparedStatementRequest,
        _request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        let statement = PreparedStatement::new(self.engine.ctx().session(), &query.query).await?;
        let handle = statement.handle().to_string();
        let parameter_schema = match statement.parameter_schema()? {
            Some(schema) => {
                let message: IpcMessage = SchemaAsIpc::new(&schema, &IpcWriteOptions::default())
                    .try_into()
                    .map_err(|e| status!("Unable to serialize schema", e))?;
                message.0
            }
            None => Bytes::default(),
        };

        let message = SchemaAsIpc::new(&statement.schema(), &IpcWriteOptions::default())
            .try_into()
            .map_err(|e| status!("Unable to serialize schema", e))?;
        let IpcMessage(schema_bytes) = message;

        self.statements.insert(handle.clone(), statement);

        let res = ActionCreatePreparedStatementResult {
            prepared_statement_handle: handle.into(),
            dataset_schema: schema_bytes,
            parameter_schema,
        };
        Ok(res)
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_close_prepared_statement(
        &self,
        _query: ActionClosePreparedStatementRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        Err(Status::unimplemented(
            "Implement do_action_close_prepared_statement",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_create_prepared_substrait_plan(
        &self,
        _query: ActionCreatePreparedSubstraitPlanRequest,
        _request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        Err(Status::unimplemented(
            "Implement do_action_create_prepared_substrait_plan",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_begin_transaction(
        &self,
        _query: ActionBeginTransactionRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginTransactionResult, Status> {
        Err(Status::unimplemented(
            "Implement do_action_begin_transaction",
        ))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_end_transaction(
        &self,
        _query: ActionEndTransactionRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        Err(Status::unimplemented("Implement do_action_end_transaction"))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_begin_savepoint(
        &self,
        _query: ActionBeginSavepointRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginSavepointResult, Status> {
        Err(Status::unimplemented("Implement do_action_begin_savepoint"))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_end_savepoint(
        &self,
        _query: ActionEndSavepointRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        Err(Status::unimplemented("Implement do_action_end_savepoint"))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn do_action_cancel_query(
        &self,
        _query: ActionCancelQueryRequest,
        _request: Request<Action>,
    ) -> Result<ActionCancelQueryResult, Status> {
        Err(Status::unimplemented("Implement do_action_cancel_query"))
    }

    async fn register_sql_info(&self, _id: i32, _result: &SqlInfo) {}
}